//! Everything here runs on the UI thread; network and disk work runs on
//! Tokio and comes back through `slint::spawn_local`.

mod account;
mod actions;
mod bind;
mod gapless;
mod items;
mod library;
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
    AccentChoice, Api as ApiGlobal, AppWindow, CardItem, QueueItem, RowItem, Theme,
    api::{self, Api, Page, Route, Track},
    audio::{Player, PlayerEvent, eq},
    downloads::Downloads,
    google_sign_in::GoogleSignIn,
    images, local,
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
/// In the order Settings lists them.
const SPEEDS: [f32; 6] = [0.8, 0.9, 1.0, 1.1, 1.2, 1.3];
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
    current_premium: bool,
    /// A track whose Premium stream failed and plays the free one instead.
    force_free: Option<String>,
    /// Track and position restored from the last run, used on its first play.
    resume: Option<(String, f64)>,
    /// The generation whose play is already recorded: a free-stream retry
    /// carrying on a song, so its start is not a second play.
    play_recorded: Option<u64>,
    /// The next track, queued in the player for gapless playback, and when
    /// its stream was resolved.
    queued: Option<(String, ResolvedStream, Instant)>,
    /// The player could not open the queued stream, so `unqueue` drops it. Only
    /// a flag: the event may be late, and a track queued since just resolves again.
    queued_failed: bool,
    /// The track last tried for queueing, so a failure is not retried
    /// every tick; it then loads as usual when the current one ends.
    queue_tried: Option<String>,
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

    /// Keeps the stream for `id`, dropping stale ones such as tracks skipped past.
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
    /// Whether the stored cookie has been checked this run, which the first
    /// signed-in Home does with its picture.
    cookie_checked: bool,
    downloads: Downloads,
    /// Songs waiting to download; the first is downloading while `downloading`.
    download_queue: VecDeque<Track>,
    downloading: bool,
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
    update: RefCell<Option<update::Update>>,
    update_timer: Timer,
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
                PlayerEvent::NextFailed => ("next-failed", String::new()),
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
        player.set_speed(settings.speed);
        player.set_output_device(settings.output_device.as_ref().map(|d| d.id.clone()));

        let premium = PremiumShared::new(
            settings::load_cookie(),
            settings.premium_account,
            settings.no_premium,
        );
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
            home_ahead: RefCell::new(home_ahead),
            local: Arc::default(),
            watcher: RefCell::new(None),
            local_check: Cell::new(None),
            local_shown: Cell::new(0),
            output_devices: RefCell::default(),
            tray: OnceCell::new(),
            hide_pending: Cell::new(false),
        });
        app.bind(ui);
        app.attach_tray();
        app.apply_settings_to_ui(ui);
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

    /// How much faster than the clock song time passes.
    fn speed(&self) -> f64 {
        f64::from(self.state.borrow().settings.speed)
    }

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
