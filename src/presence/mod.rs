use std::{
    collections::VecDeque,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use discord_rich_presence::activity::{
    Activity, ActivityType, Assets, Button, StatusDisplayType, Timestamps,
};
use serde_json::Value;
use url::Url;

use ipc::{Ipc, OP_CLOSE};

mod ipc;

const DISCORD_CLIENT_ID: &str = "1282436714408841276";

/// Coalesces the burst of events a track change produces.
const DEBOUNCE: Duration = Duration::from_secs(1);
/// Discord accepts five activity updates per 20 seconds.
const RATE_LIMIT_COUNT: usize = 5;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(20);
/// Pear clears the activity after ten minutes paused.
const PAUSED_CLEAR_AFTER: Duration = Duration::from_secs(10 * 60);
/// While an activity is shown, pings Discord this often so a restarted
/// client gets the activity back without waiting for the next player event.
const HEARTBEAT: Duration = Duration::from_secs(15);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// Start times closer than this are the same playback position, matching
/// Pear's seek threshold.
const SEEK_TOLERANCE_MS: i64 = 2_000;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Discord validates text as JavaScript strings, so limits are in UTF-16 code
/// units: emoji and other astral characters count twice.
const TEXT_MIN_UNITS: usize = 2;
const TEXT_MAX_UNITS: usize = 128;
const TEXT_FILLER: char = '\u{3164}';
const ELLIPSIS: char = '…';
const URL_MAX_CHARS: usize = 256;
const PAUSED_TEXT: &str = "⏸︎";
const LISTEN_BUTTON_LABEL: &str = "Listen on YouTube Music";
const WATCH_URL: &str = "https://music.youtube.com/watch?v=";

const EVENT_ERROR: &str = "ERROR";

const VIDEO_ID_LEN: usize = 11;
/// Longer than any track or podcast episode YouTube Music serves.
const MAX_DURATION_SECS: f64 = 24.0 * 60.0 * 60.0;
/// currentTime can run marginally past duration as a track ends.
const POSITION_SLACK_SECS: f64 = 5.0;
const ARTWORK_DOMAINS: &[&str] = &["ytimg.com", "googleusercontent.com"];

/// What is playing, as the player reports it.
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub artwork_url: Option<String>,
    pub duration: f64,
    pub position: f64,
    pub paused: bool,
}

#[derive(Clone)]
struct Presence {
    url: String,
    title: String,
    artist: String,
    album: Option<String>,
    artwork_url: Option<String>,
    paused: bool,
    /// Start and end in Unix milliseconds, absent while paused.
    span: Option<(i64, i64)>,
}

enum Message {
    Update(Option<Presence>),
    Shutdown(SyncSender<()>),
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn clamp_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut clamped = if utf16_len(text) <= TEXT_MAX_UNITS {
        text.to_owned()
    } else {
        let budget = TEXT_MAX_UNITS - ELLIPSIS.len_utf16();
        let mut used = 0;
        let end = text
            .char_indices()
            .find_map(|(i, c)| {
                used += c.len_utf16();
                (used > budget).then_some(i)
            })
            .unwrap_or(text.len());
        format!("{}{ELLIPSIS}", &text[..end])
    };
    let short_by = TEXT_MIN_UNITS.saturating_sub(utf16_len(&clamped));
    clamped.extend(std::iter::repeat_n(TEXT_FILLER, short_by));
    Some(clamped)
}

fn is_video_id(id: &str) -> bool {
    id.len() == VIDEO_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Accepts https URLs on YouTube's image hosts only, as Discord fetches the
/// artwork itself.
fn trusted_artwork_url(raw: &str) -> Option<String> {
    if raw.len() > URL_MAX_CHARS {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    crate::http::within(&url, ARTWORK_DOMAINS).then(|| url.into())
}

impl Track {
    fn validate(self, now_ms: i64) -> Result<Presence, &'static str> {
        if !is_video_id(&self.video_id) {
            return Err("malformed video ID");
        }
        if !(0.0..=MAX_DURATION_SECS).contains(&self.duration) {
            return Err("duration out of range");
        }
        let max_position = if self.duration > 0.0 {
            self.duration + POSITION_SLACK_SECS
        } else {
            MAX_DURATION_SECS
        };
        if !(0.0..=max_position).contains(&self.position) {
            return Err("position out of range");
        }
        let title = clamp_text(&self.title).ok_or("missing title")?;
        let artist = clamp_text(&self.artist).ok_or("missing artist")?;

        let artwork_url = self.artwork_url.as_deref().and_then(|raw| {
            let trusted = trusted_artwork_url(raw);
            if trusted.is_none() {
                log::debug!("dropped untrusted artwork URL");
            }
            trusted
        });
        let span = (!self.paused && self.duration > 0.0).then(|| {
            let start = now_ms - (self.position * 1000.0) as i64;
            (start, start + (self.duration * 1000.0) as i64)
        });
        Ok(Presence {
            url: format!("{WATCH_URL}{}", self.video_id),
            title,
            artist,
            album: self.album.as_deref().and_then(clamp_text),
            artwork_url,
            paused: self.paused,
            span,
        })
    }
}

impl Presence {
    fn looks_like(&self, other: &Self) -> bool {
        let spans_match = match (self.span, other.span) {
            (None, None) => true,
            (Some((a_start, a_end)), Some((b_start, b_end))) => {
                (a_start - b_start).abs() <= SEEK_TOLERANCE_MS
                    && (a_end - b_end).abs() <= SEEK_TOLERANCE_MS
            }
            _ => false,
        };
        spans_match
            && self.url == other.url
            && self.title == other.title
            && self.artist == other.artist
            && self.album == other.album
            && self.artwork_url == other.artwork_url
            && self.paused == other.paused
    }

    fn activity(&self) -> Activity<'_> {
        let large_text = if self.paused {
            Some(PAUSED_TEXT)
        } else {
            self.album.as_deref()
        };
        let mut assets = Assets::new();
        if let Some(url) = &self.artwork_url {
            assets = assets.large_image(url.as_str());
        }
        if let Some(text) = large_text {
            assets = assets.large_text(text);
        }

        let activity = Activity::new()
            .activity_type(ActivityType::Listening)
            .status_display_type(StatusDisplayType::Details)
            .details(self.title.as_str())
            .details_url(self.url.as_str())
            .state(self.artist.as_str())
            .assets(assets)
            .buttons(vec![Button::new(LISTEN_BUTTON_LABEL, self.url.as_str())]);
        match self.span {
            Some((start, end)) => activity.timestamps(Timestamps::new().start(start).end(end)),
            None => activity,
        }
    }
}

/// Owns the Discord connection on its own thread so IPC never blocks the UI.
struct Worker {
    rx: Receiver<Message>,
    client: Option<Ipc>,
    reconnect_at: Instant,
    backoff: Duration,
    wanted: Option<Presence>,
    shown: Option<Presence>,
    send_at: Option<Instant>,
    paused_since: Option<Instant>,
    recent_sends: VecDeque<Instant>,
    last_contact: Instant,
}

impl Worker {
    fn new(rx: Receiver<Message>) -> Self {
        let now = Instant::now();
        Self {
            rx,
            client: None,
            reconnect_at: now,
            backoff: RECONNECT_MIN,
            wanted: None,
            shown: None,
            send_at: None,
            paused_since: None,
            recent_sends: VecDeque::with_capacity(RATE_LIMIT_COUNT),
            last_contact: now,
        }
    }

    fn run(mut self) {
        loop {
            let message = match self.next_wake() {
                Some(at) => self
                    .rx
                    .recv_timeout(at.saturating_duration_since(Instant::now())),
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match message {
                Ok(Message::Update(presence)) => {
                    self.wanted = presence;
                    self.send_at = Some(Instant::now() + DEBOUNCE);
                }
                Ok(Message::Shutdown(ack)) => {
                    self.clear();
                    let _ = ack.send(());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.clear();
                    return;
                }
            }
            self.tick();
        }
    }

    fn next_wake(&self) -> Option<Instant> {
        if self.client.is_none() {
            return self.wanted.as_ref().map(|_| self.reconnect_at);
        }
        let send = self.send_at.map(|at| at.max(self.rate_limit_free_at()));
        let clear = self.paused_since.map(|since| since + PAUSED_CLEAR_AFTER);
        let heartbeat = self.shown.as_ref().map(|_| self.last_contact + HEARTBEAT);
        send.into_iter().chain(clear).chain(heartbeat).min()
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.client.is_none() {
            if self.wanted.is_none() || now < self.reconnect_at {
                return;
            }
            self.connect(now);
        }
        if self
            .paused_since
            .is_some_and(|since| now >= since + PAUSED_CLEAR_AFTER)
        {
            self.clear();
        }
        if self.send_at.is_some_and(|at| now >= at) && now >= self.rate_limit_free_at() {
            self.sync(now);
        }
        if self.shown.is_some() && now >= self.last_contact + HEARTBEAT {
            self.command(Ipc::ping);
        }
    }

    fn rate_limit_free_at(&self) -> Instant {
        match self.recent_sends.front() {
            Some(&oldest) if self.recent_sends.len() >= RATE_LIMIT_COUNT => {
                oldest + RATE_LIMIT_WINDOW
            }
            _ => Instant::now(),
        }
    }

    fn connect(&mut self, now: Instant) {
        match Ipc::connect(DISCORD_CLIENT_ID) {
            Ok(client) => {
                log::debug!("connected to Discord");
                self.client = Some(client);
                self.backoff = RECONNECT_MIN;
                self.shown = None;
                self.send_at = Some(now);
            }
            Err(e) => {
                log::debug!("Discord unavailable, retrying in {:?}: {e}", self.backoff);
                self.schedule_reconnect(now);
            }
        }
    }

    fn schedule_reconnect(&mut self, now: Instant) {
        self.client = None;
        self.paused_since = None;
        self.reconnect_at = now + self.backoff;
        self.backoff = (self.backoff * 2).min(RECONNECT_MAX);
    }

    fn sync(&mut self, now: Instant) {
        self.send_at = None;
        let Some(wanted) = &self.wanted else {
            self.clear();
            return;
        };
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.looks_like(wanted))
        {
            return;
        }
        let wanted = wanted.clone();

        while self
            .recent_sends
            .front()
            .is_some_and(|&sent| now >= sent + RATE_LIMIT_WINDOW)
        {
            self.recent_sends.pop_front();
        }
        self.recent_sends.push_back(now);

        let accepted = self.command(|client| client.set_activity(Some(&wanted.activity())));
        if accepted {
            match wanted.span {
                Some((start, end)) => log::debug!(
                    "activity: {} at {}s of {}s",
                    wanted.title,
                    (unix_millis() - start) / 1000,
                    (end - start) / 1000
                ),
                None => log::debug!("activity: {} (paused)", wanted.title),
            }
        }
        self.paused_since = match (accepted && wanted.paused, self.paused_since) {
            (true, Some(since)) => Some(since),
            (true, None) => Some(now),
            (false, _) => None,
        };
        self.shown = accepted.then_some(wanted);
    }

    fn clear(&mut self) {
        self.paused_since = None;
        if self.shown.take().is_some() {
            self.command(|client| client.set_activity(None));
        }
    }

    /// Sends one command and drains Discord's reply. Returns whether Discord
    /// accepted it; a broken connection schedules a reconnect.
    fn command(&mut self, send: impl FnOnce(&mut Ipc) -> std::io::Result<(u32, Value)>) -> bool {
        let Some(client) = self.client.as_mut() else {
            return false;
        };
        let reply = send(client);
        let now = Instant::now();
        self.last_contact = now;
        match reply {
            Ok((OP_CLOSE, body)) => {
                log::warn!("Discord closed the connection: {body}");
                self.schedule_reconnect(now);
                false
            }
            Ok((_, body)) if body["evt"] == EVENT_ERROR => {
                log::warn!("Discord rejected the activity: {}", body["data"]);
                false
            }
            Ok(_) => true,
            Err(e) => {
                log::debug!("lost connection to Discord: {e}");
                self.schedule_reconnect(now);
                false
            }
        }
    }
}

pub struct PresenceHandle {
    tx: Sender<Message>,
}

impl PresenceHandle {
    pub fn spawn() -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("discord-presence".into())
            .spawn(move || Worker::new(rx).run())?;
        Ok(Self { tx })
    }

    /// Asks the worker to clear the activity, waiting briefly so the clear
    /// reaches Discord before the process exits.
    pub fn shutdown(&self) {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self.tx.send(Message::Shutdown(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(SHUTDOWN_TIMEOUT);
        }
    }

    /// Shows `track`, or clears the activity for `None`.
    pub fn update(&self, track: Option<Track>) {
        let update = match track.map(|t| t.validate(unix_millis())).transpose() {
            Ok(update) => update,
            Err(reason) => {
                log::debug!("skipped presence update: {reason}");
                return;
            }
        };
        // Only fails once the worker has shut down during exit.
        let _ = self.tx.send(Message::Update(update));
    }
}

#[cfg(test)]
mod tests;
