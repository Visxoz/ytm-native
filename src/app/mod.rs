//! Everything here runs on the UI thread; network and disk work runs on
//! Tokio and comes back through `slint::spawn_local`.

mod account;
mod actions;
mod bind;
mod gapless;
mod items;
mod library;
mod mini;
mod navigation;
mod now_playing;
mod offline;
mod page;
mod playback;
mod queue_panel;
mod resolve;
mod scrobble;
mod session;
mod smart_shuffle;
mod soak;
mod song_version;
mod updates;
mod window;

use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use slint::{
    ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
    winit_030::{WinitWindowAccessor, winit},
};
use tokio::runtime::Handle;

use crate::{
    AccentChoice, Api as ApiGlobal, AppWindow, CardItem, MiniPlayer, QueueItem, RowItem, Theme,
    api::{self, Api, Page, Route, Track},
    audio::{Player, PlayerEvent, eq},
    downloads::Downloads,
    google_sign_in::GoogleSignIn,
    images, local,
    media_controls::{Action, Controls},
    presence::PresenceHandle,
    queue::{Queue, Repeat},
    settings::{self, Accent, CROSSFADE_SECS, Look, Settings},
    streams::ResolvedStream,
    update,
};
pub(crate) use mini::window_attributes as mini_window_attributes;
use resolve::PremiumShared;
use song_version::SongVersion;

const TICK: Duration = Duration::from_millis(250);
/// A row's artwork still waiting once this many more rows have asked is for
/// one scrolled past, as a screen shows far fewer.
const RECENT_ASKS: u64 = 64;
/// In the order Settings lists them.
const SPEEDS: [f32; 6] = [0.8, 0.9, 1.0, 1.1, 1.2, 1.3];
/// Equaliser animation step; a few frames a second keeps repaints cheap.
const EQ_FRAME: Duration = Duration::from_millis(125);
const TOAST_FOR: Duration = Duration::from_secs(5);
/// After the first page has had the network.
const AFTER_STARTUP: Duration = Duration::from_secs(10);
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(12 * 60 * 60);
/// As often as an open YouTube tab renews its session cookies.
const ROTATE_COOKIES_EVERY: Duration = Duration::from_secs(10 * 60);

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
    current_premium: bool,
    /// The current track's stream and when it was resolved, for a replay to
    /// take from the player's memory (`App::held_stream`).
    playing: Option<(String, ResolvedStream, Instant)>,
    /// A track whose Premium stream failed and plays the free one instead.
    force_free: Option<String>,
    /// Where a track is to start: restored from the last run, sought to
    /// while stopped, or where a Premium retry or dropped connection left
    /// off. Kept until it sounds, so a load that fails starts there again.
    resume: Option<(String, f64)>,
    /// The generation whose play is already recorded: a free-stream retry
    /// carrying on a song, so its start is not a second play.
    play_recorded: Option<u64>,
    listening: session::Listening,
    /// The next track, queued in the player for gapless playback, and when
    /// its stream was resolved.
    queued: Option<(String, ResolvedStream, Instant)>,
    /// The player could not open the queued stream, so `unqueue` drops it. Only
    /// a flag: the event may be late, and a track queued since just resolves again.
    queued_failed: bool,
    /// The track last tried for queueing, so a failure is not retried
    /// every tick; it then loads as usual when the current one ends.
    queue_tried: Option<String>,
    /// A song whose connection dropped, and the tries at opening it again.
    reopen: Option<(String, u32)>,
}

impl Playback {
    /// Forgets the track queued in the player, so the next one is queued
    /// afresh. The player's own copy is dropped with `Player::clear_next`.
    fn forget_queued(&mut self) {
        self.queued = None;
        self.queued_failed = false;
        self.queue_tried = None;
    }

    /// `forget_queued`, but keeps the stream as prefetched so it isn't asked for
    /// again. Not for a quality change or a Premium retry, which must drop it.
    fn unqueue(&mut self) {
        if let Some((id, stream, at)) = self.queued.take()
            && !self.queued_failed
        {
            self.prefetched.insert(id, (stream, at));
        }
        self.forget_queued();
    }

    fn has_prefetched(&self, id: &str) -> bool {
        self.prefetched
            .get(id)
            .is_some_and(|(_, at)| at.elapsed() < playback::PREFETCH_FRESH)
    }

    fn take_prefetched(&mut self, id: &str) -> Option<(ResolvedStream, Instant)> {
        self.prefetched
            .remove(id)
            .filter(|(_, at)| at.elapsed() < playback::PREFETCH_FRESH)
    }

    /// Counts a try at opening `id` again after its connection dropped,
    /// where the sound got to should it have started, and returns the tries.
    fn reopen_try(&mut self, id: &str, position: f64) -> u32 {
        if self.started {
            self.resume = Some((id.to_owned(), position));
            self.play_recorded = Some(self.generation);
        }
        let (_, tries) = match &mut self.reopen {
            Some(reopen) if reopen.0 == id => reopen,
            slot => slot.insert((id.to_owned(), 0)),
        };
        *tries += 1;
        *tries
    }

    fn reopening(&self, id: &str) -> bool {
        self.reopen.as_ref().is_some_and(|(r, _)| r == id)
    }

    /// Keeps the stream for `id`, dropping stale ones such as tracks skipped past.
    /// Only the newest keeps its first bytes; an older one's warm-up still
    /// running fills a cell nothing else holds.
    fn store_prefetched(&mut self, id: String, stream: ResolvedStream) {
        self.prefetched
            .retain(|_, (_, at)| at.elapsed() < playback::PREFETCH_FRESH);
        for (old, _) in self.prefetched.values_mut() {
            old.head = Default::default();
        }
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
    /// Bumped as the queue is replaced or cleared, so tracks fetched for one
    /// never join the next.
    queue_generation: u64,
    playback: Playback,
    lyrics_for: Option<String>,
    /// Each lyric line's start in seconds, when the lyrics shown are synced.
    lyric_starts: Vec<f64>,
    suggest_generation: u64,
    page_cache: HashMap<Route, (Page, Instant)>,
    /// Music videos looked up for a song version, by video ID.
    song_versions: HashMap<String, SongVersion>,
    history: settings::History,
    searches: settings::Searches,
    smart: smart_shuffle::Smart,
    /// Bumped as the playing song changes or is rated, so a rating fetched
    /// before then is not shown.
    rating_generation: u64,
    /// The song being saved to a playlist, and the playlists offered.
    saving: Option<(String, Vec<api::PlaylistOption>)>,
    artwork_accent: Option<slint::Color>,
    scrobbler: scrobble::Scrobbler,
    downloads: Downloads,
    /// Songs waiting to download; the first is downloading while `downloading`.
    download_queue: VecDeque<Track>,
    downloading: bool,
}

impl State {
    /// Replaces the queue, dropping Smart Shuffle's batch for the old one.
    fn set_queue(&mut self, tracks: Vec<Track>, start: usize, shuffle: bool) {
        // Chosen afresh, a song that dropped starts over.
        if self.playback.reopen.take().is_some() {
            self.playback.resume = None;
        }
        self.queue.set(tracks, start, shuffle);
        self.queue_generation += 1;
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
    eq_gains: Rc<VecModel<f32>>,
    /// Queue revision the queue model shows.
    queue_shown: Cell<Option<u64>>,
    selection: Cell<queue_panel::Selection>,
    /// How many page and queue rows have asked for their artwork.
    row_asks: Cell<u64>,
    queue_asks: Cell<u64>,
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
    update: RefCell<Option<update::Update>>,
    update_timer: Timer,
    cookie_timer: Timer,
    persist_timer: Timer,
    /// Retries scrobbles Last.fm could not take.
    scrobble_timer: Timer,
    /// The signed-in Home, asked for as the app starts, for the first visit.
    home_ahead: RefCell<Option<HomeAhead>>,
    /// The local files as last read, loaded with the first scan.
    local: Arc<Mutex<Option<local::Library>>>,
    watcher: RefCell<Option<local::Watcher>>,
    /// Set while a check after files changed runs, to whether they changed again since.
    local_check: Cell<Option<bool>>,
    /// The local library's revision Local files last showed.
    local_shown: Cell<u64>,
    /// Settings' devices after "System default": cpal ID and name.
    output_devices: RefCell<Vec<(String, String)>>,
    tray: OnceCell<crate::tray::Tray>,
    /// Set as the window minimises on its way to the tray.
    hide_pending: Cell<bool>,
    mini: RefCell<Option<MiniPlayer>>,
}

pub type HomeAhead = tokio::task::JoinHandle<api::Result<Page>>;

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
    pub fn new(
        ui: &AppWindow,
        rt: Handle,
        api: Api,
        settings: Settings,
        home_ahead: Option<HomeAhead>,
    ) -> Rc<Self> {
        let weak = ui.as_weak();
        let player = Player::spawn(move |event| {
            let (kind, message) = match event {
                PlayerEvent::Started => ("started", String::new()),
                PlayerEvent::Advanced => ("advanced", String::new()),
                PlayerEvent::NextDropped => ("next-dropped", String::new()),
                PlayerEvent::NextFailed { dropped } => {
                    ("next-failed", if dropped { "dropped" } else { "" }.into())
                }
                PlayerEvent::Ended => ("ended", String::new()),
                PlayerEvent::Error(e) => ("error", e),
                PlayerEvent::Dropped(e) => ("dropped", e),
                PlayerEvent::Unplugged => ("unplugged", String::new()),
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.invoke_player_event(kind.into(), message.into())
            });
        });
        player.set_volume(if settings.muted { 0.0 } else { settings.volume });
        player.set_normalise(settings.normalise);
        player.set_equaliser(settings.equaliser());
        player.set_crossfeed(settings.crossfeed);
        player.set_mono(settings.mono);
        player.set_balance(settings.balance);
        player.set_speed(settings.speed);
        player.set_pause_on_unplug(settings.pause_on_unplug);
        player.set_output_device(settings.output_device.as_ref().map(|d| d.id.clone()));

        let premium = PremiumShared::new(settings.premium_account, settings.no_premium);
        settings::remove_old_cookie();
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
                queue_generation: 0,
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
                downloads: Downloads::load(),
                download_queue: VecDeque::new(),
                downloading: false,
            }),
            rows: Rc::new(VecModel::default()),
            library: Rc::new(VecModel::default()),
            queue_model: Rc::new(VecModel::default()),
            eq_gains: Rc::new(VecModel::from(vec![0.0; eq::BANDS.len()])),
            queue_shown: Cell::new(None),
            selection: Cell::new(queue_panel::Selection::default()),
            row_asks: Cell::new(0),
            queue_asks: Cell::new(0),
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
            cookie_timer: Timer::default(),
            persist_timer: Timer::default(),
            scrobble_timer: Timer::default(),
            home_ahead: RefCell::new(home_ahead),
            local: Arc::default(),
            watcher: RefCell::new(None),
            local_check: Cell::new(None),
            local_shown: Cell::new(0),
            output_devices: RefCell::default(),
            tray: OnceCell::new(),
            hide_pending: Cell::new(false),
            mini: RefCell::new(None),
        });
        app.bind(ui);
        app.attach_tray();
        app.apply_settings_to_ui(ui);
        let proxy = crate::http::proxy_in_use();
        ui.set_proxy_detail(proxy_detail(proxy.is_some(), false).into());
        ui.set_proxy_address(proxy.unwrap_or_default().into());
        app.show_folders();
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
        app.show_output_device();
        let speeds: Vec<SharedString> = SPEEDS
            .iter()
            .map(|speed| format!("{speed}×").into())
            .collect();
        ui.set_speeds(ModelRc::new(VecModel::from(speeds)));
        let lengths: Vec<SharedString> = CROSSFADE_SECS
            .iter()
            .map(|secs| format!("{secs} s").into())
            .collect();
        ui.set_crossfade_lengths(ModelRc::new(VecModel::from(lengths)));
        app
    }

    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("window outlives the app")
    }

    /// Runs `work` on Tokio and `done` with its result on the UI thread.
    fn spawn<T: Outcome + Send + 'static>(
        self: &Rc<Self>,
        work: impl Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + 'static,
    ) {
        let handle = self.rt.spawn(work);
        let app = Rc::clone(self);
        let _ = slint::spawn_local(async move {
            if let Some(value) = outcome(handle.await) {
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
        self.load_image_while(url, edge, |_| true, apply);
    }

    /// `load_image` for an image skipped if, once its turn to download
    /// comes, it is no longer `wanted`.
    fn load_image_while(
        self: &Rc<Self>,
        url: Option<String>,
        edge: u32,
        wanted: impl FnOnce(&Rc<Self>) -> bool + 'static,
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
            if let Some(image) = images::load(&rt, &url, edge, || wanted(&app)).await {
                apply(&app, image);
            }
        });
    }

    /// Rows ask during Slint's layout pass, so the model is changed once it ends.
    fn on_row_art(self: &Rc<Self>, load: fn(&Rc<Self>, usize), row: i32) {
        let app = Rc::clone(self);
        Timer::single_shot(Duration::ZERO, move || load(&app, row as usize));
    }

    /// A list row's artwork, asked for by the row once on screen, put in by
    /// `apply` while the row still `wants` it. Skipped should `RECENT_ASKS`
    /// more rows of the list (`asks`) ask before its download's turn comes.
    fn load_row_image(
        self: &Rc<Self>,
        url: Option<String>,
        asks: fn(&Self) -> &Cell<u64>,
        wants: impl Fn(&Rc<Self>) -> bool + Clone + 'static,
        apply: impl FnOnce(&Rc<Self>, Image) + 'static,
    ) {
        let ask = asks(self).get() + 1;
        asks(self).set(ask);
        let still = wants.clone();
        self.load_image_while(
            url,
            images::ROW,
            move |app| asks(app).get() - ask < RECENT_ASKS && still(app),
            move |app, image| {
                if wants(app) {
                    apply(app, image);
                }
            },
        );
    }

    /// How much faster than the clock song time passes.
    fn speed(&self) -> f64 {
        f64::from(self.state.borrow().settings.speed)
    }

    fn show_playing(&self, playing: bool) {
        self.ui().global::<ApiGlobal>().set_is_playing(playing);
        self.sync_mini();
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
        ui.set_normalise(settings.normalise.index());
        ui.set_discord(settings.discord);
        ui.set_autoplay(settings.autoplay);
        ui.set_youtube_history(settings.youtube_history);
        ui.set_rating_buttons(settings.rating_buttons);
        ui.set_quality(settings.quality.index());
        ui.set_crossfade(settings.crossfade);
        ui.set_crossfade_secs(
            CROSSFADE_SECS
                .iter()
                .position(|&s| s == settings.crossfade_secs)
                .map_or(-1, |i| i as i32),
        );
        ui.set_pause_on_unplug(settings.pause_on_unplug);
        ui.set_speed(
            SPEEDS
                .iter()
                .position(|&s| s == settings.speed)
                .map_or(-1, |i| i as i32),
        );
        ui.set_close_to_tray(settings.close_to_tray);
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
        ui.set_crossfeed(settings.crossfeed);
        ui.set_mono(settings.mono);
        ui.set_balance(settings.balance);
        ui.set_accent_choice(settings.accent.index());
    }

    /// Lists the music folders, and lets their files be read; run as they change.
    fn show_folders(&self) {
        let folders = &self.state.borrow().settings.local_folders;
        crate::local::set_folders(folders);
        self.watcher.take();
        let ui = self.ui.clone();
        *self.watcher.borrow_mut() = local::watch(folders, move || {
            let _ = ui.upgrade_in_event_loop(|ui| ui.invoke_local_files_changed());
        });
        let names: Vec<SharedString> = folders.iter().map(|f| s(f.to_string_lossy())).collect();
        self.ui()
            .set_local_folders(ModelRc::new(VecModel::from(names)));
    }

    /// Colours the window and its title bar. Run only where the look changes,
    /// as it makes Windows restyle the window; the accent alone goes through
    /// `apply_accent`.
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
            .unwrap_or_else(|| opaque(settings::LILY));
        ui.global::<Theme>()
            .set_accent(images::readable(accent, settings.look == Look::Light));
        drop(state);
        self.theme_mini();
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

    /// Changes settings shown at once, saving only once `save`, as a slider
    /// sends every move of a drag.
    fn change_settings(&self, save: bool, change: impl FnOnce(&mut Settings)) {
        if save {
            self.update_settings(change);
        } else {
            change(&mut self.state.borrow_mut().settings);
            self.apply_settings_to_ui(&self.ui());
        }
    }

    fn change_equaliser(&self, save: bool, change: impl FnOnce(&mut Settings)) {
        self.change_settings(save, change);
        self.player
            .set_equaliser(self.state.borrow().settings.equaliser());
    }

    /// Probed on a thread of their own, as cpal sets up COM on it.
    fn list_output_devices(self: &Rc<Self>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::audio::output_devices());
        });
        self.spawn(
            async move { rx.await.unwrap_or_default() },
            |app, devices| {
                *app.output_devices.borrow_mut() = devices;
                app.show_output_device();
            },
        );
    }

    fn show_output_device(&self) {
        let ui = self.ui();
        let devices = self.output_devices.borrow();
        let chosen = &self.state.borrow().settings.output_device;
        let current = chosen.as_ref().map_or(Some(0), |chosen| {
            devices
                .iter()
                .position(|(id, _)| *id == chosen.id)
                .map(|i| i + 1)
        });
        let detail = match chosen {
            None => "Follows the default in Windows.".to_owned(),
            // Plugged in, or not listed yet.
            Some(chosen) if current.is_some() || devices.is_empty() => chosen.name.clone(),
            Some(chosen) => format!(
                "{} isn't plugged in, so the default plays until it is.",
                chosen.name
            ),
        };
        let names: Vec<SharedString> = std::iter::once(s("System default"))
            .chain(devices.iter().map(|(_, name)| s(name)))
            .collect();
        ui.set_output_devices(ModelRc::new(VecModel::from(names)));
        ui.set_output_device(current.map_or(-1, |i| i as i32));
        ui.set_output_detail(detail.into());
    }

    /// Saves `address` as the proxy for the next start, returning why it
    /// won't do, or nothing.
    fn set_proxy(&self, address: &str) -> SharedString {
        match crate::http::save_proxy(address) {
            Ok((saved, pending)) => {
                let ui = self.ui();
                ui.set_proxy_detail(proxy_detail(saved.is_some(), pending).into());
                ui.set_proxy_address(saved.unwrap_or_default().into());
                SharedString::new()
            }
            Err(e) => e.into(),
        }
    }

    /// Plays through the device Settings lists at `index`, 0 being the default.
    fn choose_output_device(&self, index: usize) {
        let device = match index.checked_sub(1) {
            None => None,
            Some(i) => match self.output_devices.borrow().get(i) {
                Some((id, name)) => Some(settings::OutputDevice {
                    id: id.clone(),
                    name: name.clone(),
                }),
                None => return,
            },
        };
        if device == self.state.borrow().settings.output_device {
            return;
        }
        self.player
            .set_output_device(device.as_ref().map(|d| d.id.clone()));
        self.update_settings(|s| s.output_device = device);
        self.show_output_device();
    }

    /// Starts background work once the event loop runs.
    pub fn start(self: &Rc<Self>) {
        images::set_scale(self.ui().window().scale_factor());
        self.restore_session();
        // Files deleted while the app was closed.
        self.local_files_changed();
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
        // Home, opened below, loads the account's picture, and the first
        // signed-in answer that comes back signed out has the cookie checked.
        self.watch_sign_in();
        if signed_in {
            // Last run's playlists and picture show while fresh ones load.
            self.show_library(&settings::load_library());
            let photo = self.state.borrow().settings.account_photo.clone();
            if let Some(url) = photo {
                self.show_account_photo(url);
            }
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
        // Once at startup, then again on their timers for a copy left running.
        let weak = Rc::downgrade(self);
        Timer::single_shot(AFTER_STARTUP, move || {
            if let Some(app) = weak.upgrade() {
                app.check_for_update();
                app.rotate_cookies();
            }
        });
        let weak = Rc::downgrade(self);
        self.update_timer
            .start(TimerMode::Repeated, UPDATE_CHECK_EVERY, move || {
                if let Some(app) = weak.upgrade() {
                    app.check_for_update();
                }
            });
        let weak = Rc::downgrade(self);
        self.cookie_timer
            .start(TimerMode::Repeated, ROTATE_COOKIES_EVERY, move || {
                if let Some(app) = weak.upgrade() {
                    app.rotate_cookies();
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

    fn shutdown(&self) {
        // Windows would leave the icon until the pointer passes over it.
        if let Some(tray) = self.tray.get() {
            tray.set_visible(false);
        }
        self.persist();
        self.player.stop();
        self.presence.shutdown();
    }
}

/// What the proxy row says of the one saved, which is `pending` until the
/// next start, as the clients took theirs at startup.
fn proxy_detail(saved: bool, pending: bool) -> &'static str {
    match (saved, pending) {
        (true, false) => "Connects through this proxy.",
        (true, true) => "Connects through this proxy from Lily's next start.",
        (false, false) => {
            "Follows the proxy set in Windows. Enter an http:// or socks5:// address to use another."
        }
        (false, true) => "Follows the proxy set in Windows from Lily's next start.",
    }
}

/// What `App::spawn`'s `done` is given should its work panic; none skips it.
trait Outcome: Sized {
    fn panicked() -> Option<Self> {
        None
    }
}

/// A failed request, so a page shows its error rather than waiting.
impl<T> Outcome for api::Result<T> {
    fn panicked() -> Option<Self> {
        Some(Err("something went wrong".to_owned()))
    }
}

/// The personal Home's failure, which falls back to the charts.
impl Outcome for Result<Page, Option<Vec<api::Section>>> {
    fn panicked() -> Option<Self> {
        Some(Err(None))
    }
}

impl<T> Outcome for Result<T, crate::lastfm::Error> {
    fn panicked() -> Option<Self> {
        Some(Err(crate::lastfm::Error::Failed(
            "something went wrong".to_owned(),
        )))
    }
}

/// No answer, so the sign-in check is armed again.
impl<T> Outcome for Option<T> {
    fn panicked() -> Option<Self> {
        Some(None)
    }
}

// Nothing to fail with, so these skip `done`. A `spawn_blocking` handle's
// panic already arrives as its `Err`.
impl Outcome for () {}
impl<T> Outcome for Vec<T> {}
impl<A, B> Outcome for (A, B) {}
impl<T> Outcome for Result<T, tokio::task::JoinError> {}

/// The panic is already logged by the hook; a task cancelled at shutdown ends
/// nothing.
fn outcome<T: Outcome>(joined: Result<T, tokio::task::JoinError>) -> Option<T> {
    match joined {
        Ok(value) => Some(value),
        Err(e) if e.is_panic() => T::panicked(),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_request_fails_as_a_failed_one_would() {
        fn fails() -> api::Result<u8> {
            panic!("a test panic")
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let joined = rt.block_on(rt.spawn(async { fails() }));
        assert_eq!(
            outcome(joined),
            Some(Err("something went wrong".to_owned()))
        );
        let joined = rt.block_on(rt.spawn(async { panic!("a test panic") }));
        assert_eq!(outcome::<()>(joined), None, "nothing to fail with");
        let cancelled = rt.spawn(std::future::pending::<api::Result<u8>>());
        cancelled.abort();
        assert_eq!(
            outcome(rt.block_on(cancelled)),
            None,
            "cancelled, not failed"
        );
    }

    #[test]
    fn a_dropped_song_reopens_where_it_first_stopped() {
        let mut playback = Playback {
            started: true,
            ..Playback::default()
        };
        assert_eq!(playback.reopen_try("a", 42.0), 1);
        assert_eq!(playback.play_recorded, Some(playback.generation));
        playback.started = false;
        playback.generation += 1;
        assert_eq!(playback.reopen_try("a", 0.0), 2, "the reopen failed too");
        assert_eq!(
            playback.resume,
            Some(("a".into(), 42.0)),
            "not where the stopped player is"
        );
        assert!(playback.reopening("a"));

        assert_eq!(playback.reopen_try("b", 9.0), 1, "another song starts over");
        assert!(!playback.reopening("a"));
        assert_eq!(
            playback.resume,
            Some(("a".into(), 42.0)),
            "b never sounded, so has no place to carry on from"
        );
    }
}
