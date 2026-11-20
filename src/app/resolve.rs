//! Chooses between the Premium and the free stream for a track, and keeps
//! the Premium path's state across songs.

use std::{
    future::Future,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use super::App;
use crate::{
    api::{self, Api},
    audio::{Input, Opening, Refresh, Source, StreamSpec},
    settings::Quality,
    streams::{self, ResolvedStream, premium},
};

pub(super) const PREMIUM_UNKNOWN: u8 = 0;
pub(super) const PREMIUM_YES: u8 = 1;
pub(super) const PREMIUM_NO: u8 = 2;
/// Songs in a row without Premium formats before an account not yet known
/// to have Premium counts as without it, as some songs simply have no
/// Premium version.
const NOT_PREMIUM_AFTER: u8 = 3;
/// The same for an account known to have Premium: only a long run of
/// uploads reaches it, and a lapsed subscription soon does.
const LAPSED_AFTER: u8 = 20;
/// Cool-off after the player fails to prepare or Premium keeps failing,
/// between background checks of an account taken as without Premium, and
/// before a failing Premium stream has the player checked again.
const PREMIUM_BACKOFF: Duration = Duration::from_secs(10 * 60);
const PREMIUM_MAX_ERRORS: u8 = 3;
/// How often the held player is checked against YouTube's current one.
const PLAYER_RECHECK: Duration = Duration::from_secs(60 * 60);

/// Premium audio state shared with background tasks.
pub(super) struct PremiumShared {
    /// The cut-down player script, swapped when YouTube moves to a new one.
    player: tokio::sync::Mutex<Option<Arc<premium::Player>>>,
    /// When the held player was loaded or last checked.
    player_checked: Mutex<Instant>,
    /// A Premium stream failed since the last check, which an outdated
    /// player can cause.
    player_suspect: AtomicBool,
    status: AtomicU8,
    /// Songs in a row that offered no Premium formats.
    misses: AtomicU8,
    /// Premium requests in a row that failed outright.
    errors: AtomicU8,
    /// When preparing the player last failed, to avoid refetching it per song.
    load_failed: Mutex<Option<Instant>>,
    /// The Premium path is skipped until then, after repeated errors.
    paused_until: Mutex<Option<Instant>>,
    /// When an account taken as without Premium is next checked in the
    /// background, as a run of music videos can make a Premium one look so.
    recheck_at: Mutex<Option<Instant>>,
}

impl PremiumShared {
    /// `known`: served Premium formats before; `none`: last taken as without
    /// Premium, which is rechecked on the first song as `recheck_at` starts unset.
    pub(super) fn new(known: bool, none: bool) -> Self {
        let status = match (known, none) {
            (true, _) => PREMIUM_YES,
            (false, true) => PREMIUM_NO,
            (false, false) => PREMIUM_UNKNOWN,
        };
        Self {
            player: tokio::sync::Mutex::new(None),
            player_checked: Mutex::new(Instant::now()),
            player_suspect: AtomicBool::new(false),
            status: AtomicU8::new(status),
            misses: AtomicU8::new(0),
            errors: AtomicU8::new(0),
            load_failed: Mutex::new(None),
            paused_until: Mutex::new(None),
            recheck_at: Mutex::new(None),
        }
    }

    pub(super) fn status(&self) -> u8 {
        self.status.load(Ordering::Relaxed)
    }

    /// Signing in or out says nothing yet about Premium.
    pub(super) fn reset(&self) {
        self.status.store(PREMIUM_UNKNOWN, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        *self.recheck_at.lock().unwrap() = None;
    }

    /// Whether to go straight to the free stream.
    fn skipped(&self) -> bool {
        self.status() == PREMIUM_NO
            || self
                .paused_until
                .lock()
                .unwrap()
                .is_some_and(|until| Instant::now() < until)
    }

    /// Whether an account taken as without Premium is due a background
    /// check, claiming it so only one runs.
    fn take_recheck(&self) -> bool {
        if self.status() != PREMIUM_NO {
            return false;
        }
        let now = Instant::now();
        let mut at = self.recheck_at.lock().unwrap();
        if at.is_some_and(|at| now < at) {
            return false;
        }
        *at = Some(now + PREMIUM_BACKOFF);
        true
    }

    fn record_premium(&self) {
        self.status.store(PREMIUM_YES, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
    }

    fn record_miss(&self) {
        let misses = self
            .misses
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let limit = if self.status() == PREMIUM_YES {
            LAPSED_AFTER
        } else {
            NOT_PREMIUM_AFTER
        };
        if misses >= limit {
            self.status.store(PREMIUM_NO, Ordering::Relaxed);
            *self.recheck_at.lock().unwrap() = Some(Instant::now() + PREMIUM_BACKOFF);
        }
    }

    /// Whether the held player is due a check against YouTube's current
    /// one, hourly or sooner after a Premium failure, claiming it so only
    /// one runs.
    fn take_player_check(&self, now: Instant) -> bool {
        let mut checked = self.player_checked.lock().unwrap();
        let age = now.saturating_duration_since(*checked);
        if age < PLAYER_RECHECK
            && !(age >= PREMIUM_BACKOFF && self.player_suspect.load(Ordering::Relaxed))
        {
            return false;
        }
        *checked = now;
        self.player_suspect.store(false, Ordering::Relaxed);
        true
    }

    /// A Premium stream failed, so check the player sooner.
    pub(super) fn suspect_player(&self) {
        self.player_suspect.store(true, Ordering::Relaxed);
    }

    fn record_error(&self) {
        self.suspect_player();
        if self.errors.fetch_add(1, Ordering::Relaxed) + 1 >= PREMIUM_MAX_ERRORS {
            self.errors.store(0, Ordering::Relaxed);
            *self.paused_until.lock().unwrap() = Some(Instant::now() + PREMIUM_BACKOFF);
            premium::diag("Premium requests keep failing; using free streams for 10 minutes");
        }
    }
}

/// `YTM_PREMIUM_DRYRUN=1` times the Premium path while playing free streams.
pub(super) fn premium_dry_run() -> bool {
    std::env::var_os("YTM_PREMIUM_DRYRUN").is_some()
}

/// The held player, checked behind the song when due, as songs a running
/// session solves with an old one start failing once YouTube moves on.
async fn premium_player(shared: &Arc<PremiumShared>) -> Result<Arc<premium::Player>, String> {
    let mut slot = shared.player.lock().await;
    if let Some(player) = slot.as_ref() {
        if shared.take_player_check(Instant::now()) {
            let (shared, held) = (Arc::clone(shared), Arc::clone(player));
            tokio::spawn(async move {
                if let Err(e) = recheck_player(&shared, &held).await {
                    premium::diag(format!("could not check the player: {e}"));
                }
            });
        }
        return Ok(Arc::clone(player));
    }
    if shared
        .load_failed
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < PREMIUM_BACKOFF)
    {
        return Err("waiting before preparing the player again".into());
    }
    match premium::load_current().await {
        Ok(player) => {
            let player = Arc::new(player);
            *slot = Some(Arc::clone(&player));
            *shared.player_checked.lock().unwrap() = Instant::now();
            Ok(player)
        }
        Err(e) => {
            *shared.load_failed.lock().unwrap() = Some(Instant::now());
            Err(e)
        }
    }
}

/// Swaps in YouTube's current player should it have moved on from `held`.
/// Songs already resolving keep the one they took.
async fn recheck_player(shared: &PremiumShared, held: &premium::Player) -> Result<(), String> {
    let id = premium::current_version().await?;
    if id != held.id {
        let player = premium::load_player(id).await?;
        premium::diag(format!("player {} replaces {}", player.id, held.id));
        *shared.player.lock().await = Some(Arc::new(player));
    }
    Ok(())
}

/// The web client's stream for `video_id` through the Premium path's
/// solver, preparing the player first: a Premium one, or with `any_format`
/// the best there is.
async fn solved_stream(
    shared: &Arc<PremiumShared>,
    video_id: &str,
    cookie: &str,
    visitor: &str,
    any_format: bool,
    max_kbps: Option<u64>,
) -> Result<ResolvedStream, String> {
    let player = premium_player(shared).await?;
    match premium::resolve(
        video_id,
        Some(cookie),
        visitor,
        &player,
        any_format,
        max_kbps,
    )
    .await?
    {
        premium::Outcome::Stream(stream) => Ok(stream),
        premium::Outcome::NoPremiumFormats => Err("no Premium formats".into()),
    }
}

/// The best stream the web client has, through the Premium path's solver,
/// for when no client hands out plain URLs.
async fn web_fallback(
    shared: &Arc<PremiumShared>,
    video_id: &str,
    cookie: &str,
    visitor: &str,
    max_kbps: Option<u64>,
) -> Result<ResolvedStream, String> {
    let stream = solved_stream(shared, video_id, cookie, visitor, true, max_kbps).await?;
    premium::diag(format!(
        "{video_id}: no client gave a plain URL; playing through the web client"
    ));
    Ok(stream)
}

fn source(stream: ResolvedStream) -> Source {
    Source {
        url: stream.url,
        size: stream.size,
        headers: stream.headers,
    }
}

/// What a track's stream is resolved with, read from the app's state on the
/// UI thread so the resolving can run, and run again, off it.
struct Resolver {
    api: Api,
    premium: Arc<PremiumShared>,
    video_id: String,
    want_premium: bool,
    max_kbps: Option<u64>,
    /// Opening a song again after its connection dropped, whose failures
    /// say nothing about Premium.
    reopening: bool,
}

impl Resolver {
    /// Premium when signed in with Premium, otherwise the first plain-URL
    /// client that answers, and signed in, the web client as a last resort.
    async fn resolve(&self) -> Result<ResolvedStream, String> {
        let Self {
            api,
            premium,
            video_id,
            want_premium,
            reopening,
            ..
        } = self;
        let video_id = video_id.as_str();
        if !api::is_video_id(video_id) {
            return Err("not a YouTube song".into());
        }
        let started = Instant::now();
        let visitor = api.visitor_data(false).await?;
        let cookie = api.cookie().filter(|_| *want_premium);
        if let Some(cookie) = cookie.as_deref().filter(|_| !premium.skipped()) {
            match premium_player(premium).await {
                Ok(player) => {
                    // A known Premium account plays a non-Premium song from this
                    // answer, saving the free clients; an unknown one counts a miss.
                    let known = premium.status() == PREMIUM_YES;
                    match premium::resolve(video_id, Some(cookie), &visitor, &player, known, None)
                        .await
                    {
                        Ok(premium::Outcome::Stream(stream)) => {
                            if stream.premium {
                                premium.record_premium();
                            } else {
                                premium.record_miss();
                            }
                            log::info!(
                                "{video_id}: {} stream resolved in {} ms",
                                if stream.premium { "Premium" } else { "web" },
                                started.elapsed().as_millis()
                            );
                            return Ok(stream);
                        }
                        Ok(premium::Outcome::NoPremiumFormats) => premium.record_miss(),
                        Err(e) => {
                            premium::diag(format!(
                                "{video_id}: Premium path failed, using the free stream: {e}"
                            ));
                            if !reopening {
                                premium.record_error();
                            }
                        }
                    }
                }
                Err(e) => premium::diag(format!("could not prepare the player: {e}")),
            }
        } else if let Some(cookie) = cookie.filter(|_| premium.take_recheck()) {
            // Checked off the playback path: this song plays the free
            // stream at once, and the next one gets Premium if it is back.
            let premium = Arc::clone(premium);
            let (video_id, visitor) = (video_id.to_owned(), visitor.clone());
            tokio::spawn(async move {
                let Ok(player) = premium_player(&premium).await else {
                    return;
                };
                match premium::resolve(&video_id, Some(&cookie), &visitor, &player, false, None)
                    .await
                {
                    Ok(premium::Outcome::Stream(_)) => premium.record_premium(),
                    Ok(premium::Outcome::NoPremiumFormats) => premium.record_miss(),
                    Err(e) => premium::diag(format!("{video_id}: Premium check failed: {e}")),
                }
            });
        } else if !want_premium
            && premium_dry_run()
            && let Ok(player) = premium_player(premium).await
        {
            // Times the Premium path without an account; the result is
            // discarded as YouTube won't serve it anonymously.
            let dry = Instant::now();
            let result = premium::resolve(video_id, None, &visitor, &player, false, None).await;
            premium::diag(format!(
                "{video_id}: dry run {} in {} ms",
                if result.is_ok() { "resolved" } else { "failed" },
                dry.elapsed().as_millis()
            ));
        }
        self.free(&visitor, None).await
    }

    /// The stream to download. Counts nothing towards the Premium state, as
    /// a playlist of music videos or uploads would read as a lapsed account.
    async fn for_keeping(&self) -> Result<ResolvedStream, String> {
        let video_id = self.video_id.as_str();
        if !api::is_video_id(video_id) {
            return Err("not a YouTube song".into());
        }
        let visitor = self.api.visitor_data(false).await?;
        let cookie = self
            .api
            .cookie()
            .filter(|_| self.want_premium && !self.premium.skipped());
        if let Some(cookie) = cookie
            && let Ok(stream) =
                solved_stream(&self.premium, video_id, &cookie, &visitor, false, None).await
        {
            return Ok(stream);
        }
        self.free(&visitor, None).await
    }

    /// A fresh URL for the same file. A free stream, web-client ones too, is
    /// asked of the plain-URL clients in format `itag`, so one googlevideo
    /// refuses still plays. Counts nothing towards the Premium counters, as
    /// the song already counted, but has the player checked sooner.
    async fn refresh(&self, was_premium: bool, itag: u32) -> Result<ResolvedStream, String> {
        let video_id = self.video_id.as_str();
        premium::diag(format!(
            "{video_id}: stream refused partway; asking for a fresh URL"
        ));
        let visitor = self.api.visitor_data(false).await?;
        if !was_premium {
            return self.free(&visitor, Some(itag)).await;
        }
        self.premium.suspect_player();
        let cookie = self.api.cookie().ok_or("signed out")?;
        solved_stream(&self.premium, video_id, &cookie, &visitor, false, None).await
    }

    /// The first plain-URL client that answers, and signed in, the web
    /// client as a last resort. `itag` holds the plain-URL clients to that
    /// format, but not the web client.
    async fn free(&self, visitor: &str, itag: Option<u32>) -> Result<ResolvedStream, String> {
        let Self {
            api,
            premium,
            video_id,
            max_kbps,
            ..
        } = self;
        let (video_id, max_kbps) = (video_id.as_str(), *max_kbps);
        let free = Instant::now();
        let result = match streams::resolve(video_id, visitor, max_kbps, itag).await {
            Ok(stream) => Ok(stream),
            // YouTube sometimes bot-checks a visitor ID; retry with a fresh one.
            Err(e) => {
                log::debug!("retrying with a new visitor ID: {e}");
                let fresh = api.visitor_data(true).await?;
                let result = streams::resolve(video_id, &fresh, max_kbps, itag).await;
                // Only a fresh ID working shows the old one was at fault;
                // both failing points at the song instead.
                if result.is_ok() {
                    api.forget_visitor(visitor);
                }
                result
            }
        };
        log::info!(
            "{video_id}: free stream resolved in {} ms",
            free.elapsed().as_millis()
        );
        match result {
            Ok(stream) => Ok(stream),
            // Signed in, the web client's scrambled URLs are a last resort,
            // read by the Premium solver whether or not the account has Premium.
            Err(e) => match api.cookie() {
                Some(cookie) => web_fallback(premium, video_id, &cookie, visitor, max_kbps)
                    .await
                    .map_err(|web| {
                        log::debug!("{video_id}: web client fallback failed: {web}");
                        e
                    }),
                None => Err(e),
            },
        }
    }
}

impl App {
    fn resolver(&self, video_id: String) -> Resolver {
        let state = self.state.borrow();
        let want_premium = state.signed_in
            && state.settings.quality == Quality::High
            && state.playback.force_free.as_deref() != Some(video_id.as_str());
        Resolver {
            api: self.api.clone(),
            premium: Arc::clone(&self.premium),
            reopening: state.playback.reopening(&video_id),
            video_id,
            want_premium,
            max_kbps: state.settings.quality.max_kbps(),
        }
    }

    /// The playing song's stream and when it was resolved, while the player
    /// holds all of it, so a replay plays from memory (its URL may have expired).
    pub(super) fn held_stream(&self, video_id: &str) -> Option<(ResolvedStream, Instant)> {
        let state = self.state.borrow();
        let (id, stream, at) = state.playback.playing.as_ref()?;
        (id == video_id && self.player.holds(&stream.url)).then(|| (stream.clone(), *at))
    }

    /// A downloaded or local file plays from there, online or not, ahead
    /// of anything prefetched.
    pub(super) fn resolve_future(
        self: &Rc<Self>,
        video_id: String,
        prefetched: Option<ResolvedStream>,
    ) -> impl Future<Output = Result<ResolvedStream, String>> + Send + 'static {
        // Only `offline_stream` vouches for a file, which may since have gone.
        let ready = (self.offline_stream(&video_id)).or(prefetched.filter(|s| s.file.is_none()));
        let resolver = self.resolver(video_id);
        async move {
            match ready {
                Some(stream) => Ok(stream),
                None => resolver.resolve().await,
            }
        }
    }

    /// `video_id`'s stream to download, from YouTube.
    pub(super) fn keeping_future(
        &self,
        video_id: String,
    ) -> impl Future<Output = Result<ResolvedStream, String>> + Send + 'static {
        let resolver = self.resolver(video_id);
        async move { resolver.for_keeping().await }
    }

    /// The player's spec for `video_id`'s stream.
    pub(super) fn stream_spec(
        &self,
        video_id: &str,
        mut stream: ResolvedStream,
        start_at: f64,
    ) -> StreamSpec {
        let opening = Opening {
            mime: std::mem::take(&mut stream.mime),
            loudness_db: stream.loudness_db,
            start_at,
        };
        let input = match stream.file.take() {
            Some(path) => Input::File(path),
            None => {
                let (source, refresh) = self.remote_source(video_id, stream);
                Input::Stream(source, refresh)
            }
        };
        StreamSpec { input, opening }
    }

    /// Where `video_id`'s stream downloads from. Should googlevideo refuse its
    /// URL partway, the download's thread waits on a fresh one, and should
    /// that be refused too, on one from another client.
    pub(super) fn remote_source(
        &self,
        video_id: &str,
        stream: ResolvedStream,
    ) -> (Source, Refresh) {
        let resolver = Arc::new(self.resolver(video_id.to_owned()));
        let (rt, was_premium, itag) = (self.rt.clone(), stream.premium, stream.itag);
        let mut client = stream.client;
        let refresh = Box::new(move |again| {
            if again && let Some(name) = client {
                streams::cut_off(name);
            }
            let resolver = Arc::clone(&resolver);
            // Spawned rather than driven here: a runtime shutting down on
            // exit drops the task and the wait errors, where this would panic.
            let task = rt.spawn(async move { resolver.refresh(was_premium, itag).await });
            let stream = rt
                .block_on(task)
                .map_err(|_| "the refresh did not finish".to_owned())??;
            client = stream.client;
            Ok(source(stream))
        });
        (source(stream), refresh)
    }

    /// Prepares the Premium player script in the background, so the first
    /// song does not wait for it. Only runs when Premium audio can apply.
    pub(super) fn warm_premium(self: &Rc<Self>) {
        let wanted = {
            let state = self.state.borrow();
            state.signed_in && state.settings.quality == Quality::High && self.api.has_cookie()
        };
        if !wanted && !premium_dry_run() {
            return;
        }
        let shared = Arc::clone(&self.premium);
        self.rt.spawn(async move {
            if let Err(e) = premium_player(&shared).await {
                premium::diag(format!("could not prepare the player: {e}"));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_long_run_of_misses_turns_off_a_known_premium_account() {
        let shared = PremiumShared::new(true, false);
        for _ in 1..LAPSED_AFTER {
            shared.record_miss();
        }
        assert_eq!(shared.status(), PREMIUM_YES, "a run of uploads");
        shared.record_premium();
        for _ in 1..LAPSED_AFTER {
            shared.record_miss();
        }
        assert_eq!(
            shared.status(),
            PREMIUM_YES,
            "a Premium song starts the count again"
        );

        shared.record_miss();
        assert_eq!(shared.status(), PREMIUM_NO, "a lapsed subscription");
        assert!(shared.skipped());
        assert!(!shared.take_recheck(), "not due until the cool-off ends");
    }

    #[test]
    fn checks_again_in_the_background_after_taking_an_account_as_without_premium() {
        let shared = PremiumShared::new(false, false);
        shared.record_miss();
        shared.record_miss();
        assert!(!shared.skipped(), "two songs say nothing yet");
        assert!(!shared.take_recheck(), "nothing to check while unknown");
        shared.record_miss();
        assert_eq!(shared.status(), PREMIUM_NO);
        assert!(shared.skipped(), "songs go straight to the free stream");
        assert!(!shared.take_recheck(), "not due yet");

        *shared.recheck_at.lock().unwrap() = Some(Instant::now());
        assert!(shared.take_recheck(), "due once the cool-off ends");
        assert!(!shared.take_recheck(), "only one check at a time");
        assert!(shared.skipped(), "songs still play free meanwhile");

        shared.record_premium();
        assert_eq!(shared.status(), PREMIUM_YES);
        assert!(!shared.skipped());
        shared.record_miss();
        assert_eq!(shared.status(), PREMIUM_YES);
    }

    #[test]
    fn checks_the_player_hourly_or_sooner_after_a_failure() {
        // Times run forward from the load, as an `Instant` before boot panics.
        let shared = PremiumShared::new(true, false);
        let loaded = *shared.player_checked.lock().unwrap();
        assert!(!shared.take_player_check(loaded), "just loaded");
        shared.suspect_player();
        assert!(
            !shared.take_player_check(loaded),
            "a failure waits out the floor"
        );

        let first = loaded + PREMIUM_BACKOFF;
        assert!(
            shared.take_player_check(first),
            "a failure brings it forward"
        );
        assert!(!shared.take_player_check(first), "only one check at a time");

        assert!(
            !shared.take_player_check(first + PREMIUM_BACKOFF),
            "the check cleared the failure"
        );
        assert!(
            shared.take_player_check(first + PLAYER_RECHECK),
            "due hourly regardless"
        );
    }

    #[test]
    fn a_new_sign_in_starts_unknown() {
        let shared = PremiumShared::new(false, false);
        for _ in 0..3 {
            shared.record_miss();
        }
        shared.reset();
        assert_eq!(shared.status(), PREMIUM_UNKNOWN);
        assert!(!shared.skipped());
    }

    #[test]
    fn a_remembered_no_plays_free_and_is_checked_at_once() {
        let shared = PremiumShared::new(false, true);
        assert!(shared.skipped(), "no Premium request before the first song");
        assert!(
            shared.take_recheck(),
            "checked in the background straight away"
        );
        assert!(!shared.take_recheck());
    }
}
