//! Ties the UI to the API, the queue and the audio engine. Everything here
//! runs on the UI thread; network and disk work is spawned on Tokio and its
//! results are picked up again through `slint::spawn_local`.

mod account;
mod actions;
mod bind;
mod gapless;
mod items;
mod library;
mod navigation;
mod now_playing;
mod page;
mod playback;
mod queue_panel;
mod resolve;
mod scrobble;
mod session;
mod smart_shuffle;
mod song_version;
mod updates;

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    future::Future,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use slint::{
    ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
    winit_030::{WinitWindowAccessor, winit},
};
use tokio::runtime::Handle;

use crate::{
    AccentChoice, Api as ApiGlobal, AppWindow, CardItem, QueueItem, RowItem, Theme,
    api::{self, Api, Page, Route, Track},
    audio::{Player, PlayerEvent, eq},
    google_sign_in::GoogleSignIn,
    images,
    media_controls::{Action, Controls},
    presence::PresenceHandle,
    queue::{Queue, Repeat},
    settings::{self, Accent, Look, Settings},
    streams::ResolvedStream,
    update,
};
use resolve::PremiumShared;
use song_version::SongVersion;

const TICK: Duration = Duration::from_millis(250);
/// Equaliser animation step; a few frames a second keeps repaints cheap.
const EQ_FRAME: Duration = Duration::from_millis(125);
const TOAST_FOR: Duration = Duration::from_secs(5);
const UPDATE_FIRST_CHECK: Duration = Duration::from_secs(10);
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(12 * 60 * 60);

struct HistoryEntry {
    route: Route,
    scroll: f32,
}

#[derive(Default)]
struct Playback {
    generation: u64,
    started: bool,
    duration: f64,
    failures: u32,
    prefetched: HashMap<String, (ResolvedStream, Instant)>,
    prefetching: HashSet<String>,
    extending: bool,
    /// When the current track was asked for, to time the first sound.
    requested_at: Option<Instant>,
    /// The current stream came from the Premium path.
    current_premium: bool,
    /// A track whose Premium stream failed and plays the free one instead.
    force_free: Option<String>,
    /// Track and position restored from the last run, used on its first play.
    resume: Option<(String, f64)>,
    /// The next track, queued in the player to follow without a gap.
    queued: Option<(String, ResolvedStream)>,
    /// The track last tried for queueing, so a failure is not retried
    /// every tick; it then loads as usual when the current one ends.
    queue_tried: Option<String>,
}

impl Playback {
    /// Forgets the track queued in the player, so the next one is queued
    /// afresh. The player's own copy is dropped with `Player::clear_next`.
    fn forget_queued(&mut self) {
        self.queued = None;
        self.queue_tried = None;
    }

    /// Whether a stream for `id` was resolved ahead and is still fresh.
    fn has_prefetched(&self, id: &str) -> bool {
        self.prefetched
            .get(id)
            .is_some_and(|(_, at)| at.elapsed() < playback::PREFETCH_FRESH)
    }

    /// Takes the stream resolved ahead for `id`, if still fresh.
    fn take_prefetched(&mut self, id: &str) -> Option<ResolvedStream> {
        self.prefetched
            .remove(id)
            .filter(|(_, at)| at.elapsed() < playback::PREFETCH_FRESH)
            .map(|(stream, _)| stream)
    }

    /// Keeps a stream resolved ahead for `id`, dropping stale ones, such as
    /// those of tracks skipped past.
    fn store_prefetched(&mut self, id: String, stream: ResolvedStream) {
        self.prefetched
            .retain(|_, (_, at)| at.elapsed() < playback::PREFETCH_FRESH);
        self.prefetched.insert(id, (stream, Instant::now()));
    }
}

struct State {
    settings: Settings,
    signed_in: bool,
    /// Bumped on every sign-in and sign-out, so a personal feed fetched for
    /// one account is never cached or saved under the next.
    account: u64,
    route: Route,
    back: Vec<HistoryEntry>,
    forward: Vec<HistoryEntry>,
    page: Page,
    page_generation: u64,
    restore_scroll: Option<f32>,
    queue: Queue<Track>,
    playback: Playback,
    lyrics_for: Option<String>,
    /// Each lyric line's start in seconds, when the lyrics shown are synced.
    lyric_starts: Vec<f64>,
    suggest_generation: u64,
    /// Recently loaded pages, so going back does not fetch them again.
    page_cache: HashMap<Route, (Page, Instant)>,
    /// Music videos looked up for a song version, by video ID.
    song_versions: HashMap<String, SongVersion>,
    /// The songs played, saved between runs.
    history: settings::History,
    /// The last few searches, offered in the search box until the user types.
    searches: settings::Searches,
    smart: smart_shuffle::Smart,
    /// Bumped as the playing song changes or is rated, so a rating fetched
    /// before then is not shown.
    rating_generation: u64,
    /// The song being saved to a playlist, and the playlists offered.
    saving: Option<(String, Vec<api::PlaylistOption>)>,
    /// The accent taken from the playing song's artwork, once it loaded.
    artwork_accent: Option<slint::Color>,
    scrobbler: scrobble::Scrobbler,
    /// Whether the stored cookie has been checked this run, which the first
    /// signed-in Home does with its picture.
    cookie_checked: bool,
}

impl State {
    /// Replaces the queue, dropping Smart Shuffle's batch for the old one.
    fn set_queue(&mut self, tracks: Vec<Track>, start: usize, shuffle: bool) {
        self.queue.set(tracks, start, shuffle);
        self.smart.reset();
    }
}

pub struct App {
    ui: slint::Weak<AppWindow>,
    rt: Handle,
    api: Api,
    player: Player,
    presence: PresenceHandle,
    media: RefCell<Option<Controls>>,
    state: RefCell<State>,
    rows: Rc<VecModel<RowItem>>,
    library: Rc<VecModel<CardItem>>,
    queue_model: Rc<VecModel<QueueItem>>,
    /// Each equaliser band's gain in dB, for its slider.
    eq_gains: Rc<VecModel<f32>>,
    /// Queue revision the queue model shows.
    queue_shown: Cell<Option<u64>>,
    selection: Cell<queue_panel::Selection>,
    tick: Timer,
    eq_timer: Timer,
    /// Lights up the next lyric line as it starts, between ticks.
    lyric_timer: Timer,
    premium: Arc<PremiumShared>,
    premium_shown: Cell<u8>,
    toast_timer: Timer,
    suggest_timer: Timer,
    google_sign_in: RefCell<Option<GoogleSignIn>>,
    sign_in_timer: Timer,
    /// A newer release found on GitHub.
    update: RefCell<Option<update::Update>>,
    update_timer: Timer,
    /// Writes the session and history a moment after they change.
    persist_timer: Timer,
    /// Sends the scrobbles again after Last.fm could not take them.
    scrobble_timer: Timer,
}

fn s(text: impl AsRef<str>) -> SharedString {
    SharedString::from(text.as_ref())
}

/// The colour `0xRRGGBB`, ignoring any alpha byte.
fn opaque(rgb: u32) -> slint::Color {
    slint::Color::from_argb_encoded(0xff00_0000 | (rgb & 0x00ff_ffff))
}

/// The local date and time, or UTC when Windows will not say the offset.
fn local_now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc())
}

impl App {
    pub fn new(ui: &AppWindow, rt: Handle, api: Api, settings: Settings) -> Rc<Self> {
        let weak = ui.as_weak();
        let player = Player::spawn(move |event| {
            let (kind, message) = match event {
                PlayerEvent::Started => ("started", String::new()),
                PlayerEvent::Advanced => ("advanced", String::new()),
                PlayerEvent::NextDropped => ("next-dropped", String::new()),
                PlayerEvent::Ended => ("ended", String::new()),
                PlayerEvent::Error(e) => ("error", e),
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.invoke_player_event(kind.into(), message.into())
            });
        });
        player.set_volume(if settings.muted { 0.0 } else { settings.volume });
        player.set_normalise(settings.normalise);
        player.set_equaliser(settings.equaliser());

        let premium = PremiumShared::new(settings::load_cookie(), settings.premium_account);
        let presence = PresenceHandle::spawn().expect("presence thread spawns");
        let mut queue = Queue::new();
        queue.repeat = Repeat::from_index(settings.repeat);

        let app = Rc::new(Self {
            ui: ui.as_weak(),
            rt,
            api,
            player,
            presence,
            media: RefCell::new(None),
            state: RefCell::new(State {
                settings,
                signed_in: false,
                route: Route::Home,
                back: Vec::new(),
                forward: Vec::new(),
                page: Page::default(),
                account: 0,
                page_generation: 0,
                restore_scroll: None,
                queue,
                playback: Playback::default(),
                lyrics_for: None,
                lyric_starts: Vec::new(),
                suggest_generation: 0,
                page_cache: HashMap::new(),
                song_versions: HashMap::new(),
                history: settings::History::load(),
                searches: settings::Searches::load(),
                smart: smart_shuffle::Smart::default(),
                rating_generation: 0,
                saving: None,
                artwork_accent: None,
                scrobbler: scrobble::Scrobbler::load(),
                cookie_checked: false,
            }),
            rows: Rc::new(VecModel::default()),
            library: Rc::new(VecModel::default()),
            queue_model: Rc::new(VecModel::default()),
            eq_gains: Rc::new(VecModel::from(vec![0.0; eq::BANDS.len()])),
            queue_shown: Cell::new(None),
            selection: Cell::new(queue_panel::Selection::default()),
            tick: Timer::default(),
            eq_timer: Timer::default(),
            lyric_timer: Timer::default(),
            premium: Arc::new(premium),
            premium_shown: Cell::new(u8::MAX),
            toast_timer: Timer::default(),
            suggest_timer: Timer::default(),
            google_sign_in: RefCell::new(None),
            sign_in_timer: Timer::default(),
            update: RefCell::new(None),
            update_timer: Timer::default(),
            persist_timer: Timer::default(),
            scrobble_timer: Timer::default(),
        });
        app.bind(ui);
        app.apply_settings_to_ui(ui);
        app.apply_theme(ui);
        // Set here and by the switch only, so other settings changing can't
        // wipe out a found update's line.
        ui.set_update_status(
            updates::up_to_date_status(app.state.borrow().settings.check_updates).into(),
        );
        ui.set_rows(ModelRc::from(Rc::clone(&app.rows)));
        ui.set_library(ModelRc::from(Rc::clone(&app.library)));
        ui.set_queue(ModelRc::from(Rc::clone(&app.queue_model)));
        ui.set_eq_gains(ModelRc::from(Rc::clone(&app.eq_gains)));
        ui.set_eq_max_db(eq::MAX_DB);
        let labels: Vec<SharedString> = eq::BANDS
            .iter()
            .map(|&hz| match hz {
                1_000.0.. => format!("{} kHz", hz / 1_000.0),
                _ => format!("{hz} Hz"),
            })
            .map(Into::into)
            .collect();
        ui.set_eq_bands(ModelRc::new(VecModel::from(labels)));
        let presets: Vec<SharedString> = eq::PRESETS.iter().map(|(name, _)| s(name)).collect();
        ui.set_eq_presets(ModelRc::new(VecModel::from(presets)));
        let accents: Vec<AccentChoice> = Accent::ALL
            .iter()
            .map(|accent| AccentChoice {
                name: s(accent.name()),
                colour: accent.rgb().map(opaque).unwrap_or_default(),
                artwork: accent.rgb().is_none(),
            })
            .collect();
        ui.set_accents(ModelRc::new(VecModel::from(accents)));
        app.show_recent_searches();
        app
    }

    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("window outlives the app")
    }

    /// Runs `work` on Tokio and `done` with its result on the UI thread.
    fn spawn<T: Send + 'static>(
        self: &Rc<Self>,
        work: impl Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + 'static,
    ) {
        let handle = self.rt.spawn(work);
        let app = Rc::clone(self);
        let _ = slint::spawn_local(async move {
            if let Ok(value) = handle.await {
                done(&app, value);
            }
        });
    }

    fn load_image(
        self: &Rc<Self>,
        url: Option<String>,
        edge: u32,
        apply: impl FnOnce(&Rc<Self>, Image) + 'static,
    ) {
        let Some(url) = url else { return };
        if let Some(image) = images::cached(&url, edge) {
            apply(self, image);
            return;
        }
        let app = Rc::clone(self);
        let rt = self.rt.clone();
        let _ = slint::spawn_local(async move {
            if let Some(image) = images::load(&rt, &url, edge).await {
                apply(&app, image);
            }
        });
    }

    /// Updates the play button and the equalisers on playing rows.
    fn show_playing(&self, playing: bool) {
        self.ui().global::<ApiGlobal>().set_is_playing(playing);
    }

    fn toast(self: &Rc<Self>, message: impl Into<String>) {
        let message = message.into();
        log::info!("{message}");
        let ui = self.ui();
        ui.set_toast(message.into());
        ui.set_toast_visible(true);
        let weak = self.ui.clone();
        self.toast_timer
            .start(TimerMode::SingleShot, TOAST_FOR, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_toast_visible(false);
                }
            });
    }

    fn apply_settings_to_ui(&self, ui: &AppWindow) {
        let state = self.state.borrow();
        let settings = &state.settings;
        ui.set_volume(settings.volume);
        ui.set_muted(settings.muted);
        ui.set_shuffle(settings.shuffle);
        ui.set_smart_shuffle(settings.smart_shuffle);
        ui.set_repeat_mode(settings.repeat);
        ui.set_normalise(settings.normalise);
        ui.set_discord(settings.discord);
        ui.set_autoplay(settings.autoplay);
        ui.set_youtube_history(settings.youtube_history);
        ui.set_rating_buttons(settings.rating_buttons);
        ui.set_quality(settings.quality.index());
        ui.set_crossfade(settings.crossfade);
        ui.set_sidebar_collapsed(settings.sidebar_collapsed);
        ui.set_check_updates(settings.check_updates);
        ui.set_equaliser(settings.equaliser);
        ui.set_eq_preset(
            eq::PRESETS
                .iter()
                .position(|(_, gains)| *gains == settings.equaliser_gains)
                .map_or(-1, |i| i as i32),
        );
        for (band, &db) in settings.equaliser_gains.iter().enumerate() {
            self.eq_gains.set_row_data(band, db);
        }
        ui.set_accent_choice(settings.accent.index());
    }

    /// Colours the window, and its title bar, in the chosen look and
    /// accent, made readable in that look. With the accent taken from the
    /// artwork, YouTube Music's red stands in until a song's has loaded, and
    /// for greyish artwork. Run where the look changes, not with every
    /// setting or song, as it makes Windows restyle the window; the accent
    /// alone goes through `apply_accent`.
    fn apply_theme(&self, ui: &AppWindow) {
        let look = self.state.borrow().settings.look;
        ui.global::<Theme>().set_look(look.index());
        self.apply_accent(ui);
        let light = look == Look::Light;
        // Windows draws the title bar. Until the native window exists this
        // does nothing, and `attach_window` applies the theme again.
        ui.window().with_winit_window(|window| {
            window.set_theme(Some(if light {
                winit::window::Theme::Light
            } else {
                winit::window::Theme::Dark
            }));
        });
    }

    /// Sets the accent alone, which leaves the title bar as it is.
    fn apply_accent(&self, ui: &AppWindow) {
        let state = self.state.borrow();
        let settings = &state.settings;
        let accent = settings
            .accent
            .rgb()
            .map(opaque)
            .or(state.artwork_accent)
            .unwrap_or_else(|| opaque(settings::YOUTUBE_RED));
        ui.global::<Theme>()
            .set_accent(images::readable(accent, settings.look == Look::Light));
    }

    /// Keeps the playing artwork's colour, recolouring the window only when
    /// the accent follows the artwork and the colour changed.
    fn set_artwork_accent(&self, accent: Option<slint::Color>) {
        let recolour = {
            let mut state = self.state.borrow_mut();
            let changed = state.artwork_accent != accent;
            state.artwork_accent = accent;
            changed && state.settings.accent == Accent::Artwork
        };
        if recolour {
            self.apply_accent(&self.ui());
        }
    }

    fn update_settings(&self, change: impl FnOnce(&mut Settings)) {
        let settings = {
            let mut state = self.state.borrow_mut();
            change(&mut state.settings);
            state.settings.clone()
        };
        settings.save();
        self.apply_settings_to_ui(&self.ui());
    }

    /// Changes the equaliser's settings and plays with them, saving them
    /// to disk if `save`.
    fn change_equaliser(&self, save: bool, change: impl FnOnce(&mut Settings)) {
        if save {
            self.update_settings(change);
        } else {
            change(&mut self.state.borrow_mut().settings);
            self.apply_settings_to_ui(&self.ui());
        }
        self.player
            .set_equaliser(self.state.borrow().settings.equaliser());
    }

    pub fn settings(&self) -> Settings {
        self.state.borrow().settings.clone()
    }

    /// Starts background work once the event loop runs.
    pub fn start(self: &Rc<Self>) {
        images::set_scale(self.ui().window().scale_factor());
        self.restore_session();
        let app = Rc::clone(self);
        self.tick
            .start(TimerMode::Repeated, TICK, move || app.on_tick());

        let weak = self.ui.clone();
        self.eq_timer.start(TimerMode::Repeated, EQ_FRAME, move || {
            if let Some(ui) = weak.upgrade() {
                let api = ui.global::<ApiGlobal>();
                if api.get_is_playing() {
                    api.set_eq_phase(api.get_eq_phase().wrapping_add(1));
                }
            }
        });

        // A stored cookie counts as signed in, so Home loads without waiting
        // for YouTube to confirm it.
        let signed_in = self.api.has_cookie();
        self.state.borrow_mut().signed_in = signed_in;
        self.ui().global::<ApiGlobal>().set_signed_in(signed_in);
        // Home, opened below, loads the account's picture and checks the
        // cookie with it.
        if signed_in {
            self.load_library();
            if let Some(id) = self.playing_id() {
                self.load_rating(id);
            }
        }
        self.warm_premium();
        self.show_lastfm();
        // Plays left over from a run that could not reach Last.fm.
        self.send_scrobbles();
        self.navigate(Route::Home, false);
        // Once at startup, after the first page has had the network, then
        // twice a day for a copy left running.
        let weak = Rc::downgrade(self);
        Timer::single_shot(UPDATE_FIRST_CHECK, move || {
            if let Some(app) = weak.upgrade() {
                app.check_for_update();
            }
        });
        let weak = Rc::downgrade(self);
        self.update_timer
            .start(TimerMode::Repeated, UPDATE_CHECK_EVERY, move || {
                if let Some(app) = weak.upgrade() {
                    app.check_for_update();
                }
            });
    }

    /// Hooks up what needs the native window, which appears some time after
    /// the event loop starts: the media overlay and the title bar's colours.
    pub fn attach_window(&self, hwnd: *mut std::ffi::c_void) {
        self.apply_theme(&self.ui());
        let weak = self.ui.clone();
        let controls = Controls::new(hwnd, move |action| {
            let (name, value) = match action {
                Action::Toggle => ("toggle", 0.0),
                Action::Play => ("play", 0.0),
                Action::Pause => ("pause", 0.0),
                Action::Next => ("next", 0.0),
                Action::Previous => ("previous", 0.0),
                Action::Stop => ("pause", 0.0),
                Action::SeekBy(by) => ("seek-by", by),
                Action::SeekTo(at) => ("seek-to", at),
            };
            let _ = weak
                .upgrade_in_event_loop(move |ui| ui.invoke_media_action(name.into(), value as f32));
        });
        *self.media.borrow_mut() = controls;
    }

    pub fn shutdown(&self) {
        self.persist();
        self.player.stop();
        self.presence.shutdown();
    }
}
