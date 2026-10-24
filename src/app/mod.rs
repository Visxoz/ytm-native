//! Ties the UI to the API, the queue and the audio engine. Everything here
//! runs on the UI thread; network and disk work is spawned on Tokio and its
//! results are picked up again through `slint::spawn_local`.

mod account;
mod actions;
mod bind;
mod gapless;
mod items;
mod navigation;
mod now_playing;
mod page;
mod playback;
mod queue_panel;
mod resolve;
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

use slint::{ComponentHandle, Image, ModelRc, SharedString, Timer, TimerMode, VecModel};
use tokio::runtime::Handle;

use crate::{
    Api as ApiGlobal, AppWindow, CardItem, QueueItem, RowItem,
    api::{Api, Page, Route, Track},
    audio::{Player, PlayerEvent},
    google_sign_in::GoogleSignIn,
    images,
    media_controls::{Action, Controls},
    presence::PresenceHandle,
    queue::{Queue, Repeat},
    settings::{self, Settings},
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

struct State {
    settings: Settings,
    signed_in: bool,
    route: Route,
    back: Vec<HistoryEntry>,
    forward: Vec<HistoryEntry>,
    page: Page,
    page_generation: u64,
    restore_scroll: Option<f32>,
    queue: Queue<Track>,
    playback: Playback,
    lyrics_for: Option<String>,
    suggest_generation: u64,
    /// Recently loaded pages, so going back does not fetch them again.
    page_cache: HashMap<Route, (Page, Instant)>,
    /// Music videos looked up for a song version, by video ID.
    song_versions: HashMap<String, SongVersion>,
    /// The songs played, saved between runs.
    history: settings::History,
    /// The queue is a collection of the user's choosing, a playlist, album
    /// or section, rather than a radio; Smart Shuffle mixes into those only.
    queue_collection: bool,
    smart: smart_shuffle::Smart,
}

impl State {
    /// Replaces the queue; see `queue_collection`.
    fn set_queue(&mut self, tracks: Vec<Track>, start: usize, shuffle: bool, collection: bool) {
        self.queue.set(tracks, start, shuffle);
        self.queue_collection = collection;
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
    /// Queue revision the queue model shows.
    queue_shown: Cell<Option<u64>>,
    selection: RefCell<queue_panel::Selection>,
    tick: Timer,
    eq_timer: Timer,
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
}

fn s(text: impl AsRef<str>) -> SharedString {
    SharedString::from(text.as_ref())
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
                page_generation: 0,
                restore_scroll: None,
                queue,
                playback: Playback::default(),
                lyrics_for: None,
                suggest_generation: 0,
                page_cache: HashMap::new(),
                song_versions: HashMap::new(),
                history: settings::History::load(),
                queue_collection: false,
                smart: smart_shuffle::Smart::default(),
            }),
            rows: Rc::new(VecModel::default()),
            library: Rc::new(VecModel::default()),
            queue_model: Rc::new(VecModel::default()),
            queue_shown: Cell::new(None),
            selection: RefCell::new(queue_panel::Selection::default()),
            tick: Timer::default(),
            eq_timer: Timer::default(),
            premium: Arc::new(premium),
            premium_shown: Cell::new(u8::MAX),
            toast_timer: Timer::default(),
            suggest_timer: Timer::default(),
            google_sign_in: RefCell::new(None),
            sign_in_timer: Timer::default(),
            update: RefCell::new(None),
            update_timer: Timer::default(),
            persist_timer: Timer::default(),
        });
        app.bind(ui);
        app.apply_settings_to_ui(ui);
        ui.set_rows(ModelRc::from(Rc::clone(&app.rows)));
        ui.set_library(ModelRc::from(Rc::clone(&app.library)));
        ui.set_queue(ModelRc::from(Rc::clone(&app.queue_model)));
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
        let ui = self.ui();
        ui.set_np_playing(playing);
        ui.global::<ApiGlobal>().set_is_playing(playing);
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
        ui.set_quality(settings.quality.index());
        ui.set_crossfade(settings.crossfade);
        ui.set_sidebar_collapsed(settings.sidebar_collapsed);
        ui.set_check_updates(settings.check_updates);
        ui.set_update_status(updates::up_to_date_status(settings.check_updates).into());
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
        self.ui().set_signed_in(signed_in);
        if signed_in {
            self.load_library();
            self.load_account_photo();
        }
        self.warm_premium();
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
        if signed_in {
            let api = self.api.clone();
            self.spawn(async move { api.cookie_valid().await }, |app, valid| {
                if valid == Some(false) {
                    app.sign_in_expired();
                }
            });
        }
    }

    pub fn attach_media_controls(&self, hwnd: *mut std::ffi::c_void) {
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
