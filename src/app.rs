//! Ties the UI to the API, the queue and the audio engine. Everything here
//! runs on the UI thread; network and disk work is spawned on Tokio and its
//! results are picked up again through `slint::spawn_local`.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    future::Future,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use tokio::runtime::Handle;

use crate::{
    Api as ApiGlobal, AppWindow, CardItem, QueueItem, RowItem, RowKind, TrackItem,
    api::{self, Api, Card, CardKind, CardLayout, Page, QueueMode, Route, Section, Track},
    audio::{Player, PlayerEvent, StreamSpec},
    google_sign_in::GoogleSignIn,
    images,
    media_controls::{Action, Controls},
    premium,
    presence::{self, PresenceHandle},
    queue::{Queue, Repeat},
    settings::{self, Settings},
    streams::{self, ResolvedStream},
    update,
};

const TICK: Duration = Duration::from_millis(250);
/// Equaliser animation step; a few frames a second keeps repaints cheap.
const EQ_FRAME: Duration = Duration::from_millis(125);
const TOAST_FOR: Duration = Duration::from_secs(5);
/// Pressing previous later than this restarts the track instead.
const RESTART_AFTER_SECS: f64 = 3.0;
/// Stream URLs last about six hours; refresh well before.
const PREFETCH_FRESH: Duration = Duration::from_secs(60 * 60);
/// How long before a track ends the next one is queued to follow it with no
/// gap: time for its stream to start downloading and its decoder to open.
const GAPLESS_LEAD_SECS: f64 = 15.0;
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const UPDATE_FIRST_CHECK: Duration = Duration::from_secs(10);
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(12 * 60 * 60);
/// How long a loaded page is reused when navigating back to it.
const PAGE_CACHE_FOR: Duration = Duration::from_secs(5 * 60);
const PAGE_CACHE_SIZE: usize = 12;
const SUGGEST_DELAY: Duration = Duration::from_millis(150);

type CardArt = (Rc<VecModel<CardItem>>, Vec<Option<String>>, u32);

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
    queue_shown: std::cell::Cell<Option<u64>>,
    tick: Timer,
    eq_timer: Timer,
    premium: Arc<PremiumShared>,
    premium_shown: std::cell::Cell<u8>,
    toast_timer: Timer,
    suggest_timer: Timer,
    google_sign_in: RefCell<Option<GoogleSignIn>>,
    sign_in_timer: Timer,
    /// A newer release found on GitHub.
    update: RefCell<Option<update::Update>>,
    update_timer: Timer,
}

fn s(text: impl AsRef<str>) -> SharedString {
    SharedString::from(text.as_ref())
}

fn track_item(track: &Track, number: usize) -> TrackItem {
    TrackItem {
        id: s(&track.id),
        title: s(&track.title),
        artists: s(track.artist_names()),
        album: s(track.album.as_ref().map_or("", |(_, name)| name.as_str())),
        duration: s(track.duration.map(api::format_duration).unwrap_or_default()),
        art: track
            .thumb
            .as_deref()
            .and_then(|url| images::cached(url, images::ROW))
            .unwrap_or_default(),
        number: s(number.to_string()),
        has_artist: track.first_artist_id().is_some(),
        has_album: track.album.is_some(),
    }
}

fn track_row(track: &Track, index: usize, numbered: bool, section: usize) -> RowItem {
    let number = if numbered {
        track.track_nr.map_or(index + 1, usize::from)
    } else {
        index + 1
    };
    RowItem {
        kind: RowKind::Track,
        track: track_item(track, number),
        numbered,
        show_album: !numbered,
        section: section as i32,
        index: index as i32,
        ..Default::default()
    }
}

fn card_item(card: &Card, edge: u32) -> CardItem {
    let tint = card.color.map_or(slint::Color::default(), |c| {
        slint::Color::from_argb_encoded(0xFF00_0000 | (c & 0x00FF_FFFF))
    });
    CardItem {
        kind: s(card.kind.as_str()),
        id: s(&card.id),
        title: s(&card.title),
        subtitle: s(&card.subtitle),
        art: card
            .thumb
            .as_deref()
            .and_then(|url| images::cached(url, edge))
            .unwrap_or_default(),
        round: card.kind == CardKind::Artist,
        tint,
    }
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
            }),
            rows: Rc::new(VecModel::default()),
            library: Rc::new(VecModel::default()),
            queue_model: Rc::new(VecModel::default()),
            queue_shown: std::cell::Cell::new(None),
            tick: Timer::default(),
            eq_timer: Timer::default(),
            premium: Arc::new(PremiumShared {
                player: tokio::sync::Mutex::new(None),
                cookie: std::sync::RwLock::new(settings::load_cookie()),
                status: AtomicU8::new(PREMIUM_UNKNOWN),
                misses: AtomicU8::new(0),
                errors: AtomicU8::new(0),
                load_failed: std::sync::Mutex::new(None),
                paused_until: std::sync::Mutex::new(None),
            }),
            premium_shown: std::cell::Cell::new(u8::MAX),
            toast_timer: Timer::default(),
            suggest_timer: Timer::default(),
            google_sign_in: RefCell::new(None),
            sign_in_timer: Timer::default(),
            update: RefCell::new(None),
            update_timer: Timer::default(),
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
        ui.set_repeat_mode(settings.repeat);
        ui.set_normalise(settings.normalise);
        ui.set_discord(settings.discord);
        ui.set_autoplay(settings.autoplay);
        ui.set_premium_audio(settings.premium_audio);
        ui.set_sidebar_collapsed(settings.sidebar_collapsed);
        ui.set_check_updates(settings.check_updates);
        ui.set_update_status(up_to_date_status(settings.check_updates).into());
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

    /// The restored position, if the current track is the restored one.
    fn take_resume_position(&self) -> f64 {
        let mut state = self.state.borrow_mut();
        let current = state.queue.current().map(|t| t.id.clone());
        match state.playback.resume.take() {
            Some((id, position)) if Some(&id) == current.as_ref() => position,
            _ => 0.0,
        }
    }

    /// Saves the queue and position so the next start can resume.
    fn save_session(&self) {
        let state = self.state.borrow();
        let Some(index) = state.queue.current_index() else {
            return;
        };
        let current = state.queue.current().map(|t| t.id.as_str());
        let position = match &state.playback.resume {
            Some((id, position)) if Some(id.as_str()) == current => *position,
            _ if state.playback.started => self.player.position(),
            _ => 0.0,
        };
        settings::Session {
            queue: state.queue.items().cloned().collect(),
            index,
            position,
            duration: state.playback.duration,
        }
        .save();
    }

    /// Shows the last run's track, paused, ready to resume where it was.
    fn restore_session(self: &Rc<Self>) {
        let Some(session) = settings::Session::load() else {
            return;
        };
        let track = session.queue[session.index].clone();
        {
            let mut state = self.state.borrow_mut();
            state.queue.set(session.queue, session.index, false);
            state.playback.resume = Some((track.id.clone(), session.position));
            state.playback.duration = track.duration.map_or(session.duration, f64::from);
        }
        let duration = self.state.borrow().playback.duration;
        let ui = self.ui();
        ui.set_has_track(true);
        ui.set_np_title(s(&track.title));
        ui.set_np_artists(s(track.artist_names()));
        ui.set_np_album(s(track.album.as_ref().map_or("", |(_, n)| n.as_str())));
        ui.set_np_has_artist(track.first_artist_id().is_some());
        ui.set_np_has_album(track.album.is_some());
        ui.set_np_duration(duration as f32);
        ui.set_np_position(session.position as f32);
        self.show_playing(false);
        ui.global::<ApiGlobal>().set_playing_id(s(&track.id));
        self.load_image(track.thumb.clone(), images::PLAYER, |app, image| {
            app.ui().set_np_art(image);
        });
        self.refresh_queue();
    }

    pub fn shutdown(&self) {
        self.save_session();
        self.player.stop();
        self.presence.shutdown();
    }

    // Wiring

    fn bind(self: &Rc<Self>, ui: &AppWindow) {
        macro_rules! on {
            ($setter:ident, |$app:ident $(, $arg:ident)*| $body:expr) => {{
                let weak = Rc::downgrade(self);
                ui.$setter(move |$($arg),*| {
                    if let Some($app) = weak.upgrade() {
                        $body
                    }
                });
            }};
        }

        on!(on_go, |app, nav| match nav.as_str() {
            "home" => app.navigate(Route::Home, true),
            "explore" => app.navigate(Route::Explore, true),
            "library" => app.navigate(Route::Library, true),
            _ => app.reload(),
        });
        on!(on_back, |app| app.go_back());
        on!(on_forward, |app| app.go_forward());
        on!(on_search, |app, query| {
            let query = query.trim().to_owned();
            app.suggest_timer.stop();
            app.state.borrow_mut().suggest_generation += 1;
            app.ui().set_suggestions(ModelRc::default());
            if !query.is_empty() {
                app.navigate(Route::Search(query), true);
            }
        });
        on!(on_search_edited, |app, query| app
            .suggest(query.to_string()));
        on!(on_toggle_play, |app| app.toggle_play());
        on!(on_next, |app| app.next(true));
        on!(on_previous, |app| app.previous());
        on!(on_seek, |app, fraction| {
            let duration = app.state.borrow().playback.duration;
            app.seek_to(fraction as f64 * duration);
        });
        on!(on_set_volume, |app, volume| app.set_volume(volume));
        on!(on_toggle_mute, |app| app.toggle_mute());
        on!(on_toggle_shuffle, |app| app.toggle_shuffle());
        on!(on_cycle_repeat, |app| app.cycle_repeat());
        on!(on_queue_play, |app, index| {
            let found = app.state.borrow_mut().queue.jump(index as usize).is_some();
            if found {
                app.start_current();
            }
        });
        on!(on_queue_remove, |app, index| {
            if app.state.borrow_mut().queue.remove(index as usize) {
                app.refresh_queue();
            }
        });
        on!(on_queue_clear, |app| {
            app.state.borrow_mut().queue.clear_upcoming();
            app.refresh_queue();
        });
        on!(on_panel_tab_changed, |app, tab| {
            if tab == 1 {
                app.load_lyrics();
            }
        });
        on!(on_np_go_artist, |app| {
            let id = app
                .state
                .borrow()
                .queue
                .current()
                .and_then(|t| t.first_artist_id().map(str::to_owned));
            if let Some(id) = id {
                app.navigate(Route::Artist(id), true);
            }
        });
        on!(on_np_go_album, |app| {
            let id = app
                .state
                .borrow()
                .queue
                .current()
                .and_then(|t| t.album.as_ref().map(|(id, _)| id.clone()));
            if let Some(id) = id {
                app.navigate(Route::Album(id), true);
            }
        });
        on!(on_sign_in, |app, cookie| app.sign_in(cookie.to_string()));
        on!(on_sign_in_browser, |app| app.sign_in_with_google());
        on!(on_sign_out, |app| app.sign_out());
        on!(on_set_normalise, |app, value| {
            app.update_settings(|s| s.normalise = value);
            app.player.set_normalise(value);
        });
        on!(on_set_discord, |app, value| {
            app.update_settings(|s| s.discord = value);
            app.update_presence();
        });
        on!(on_sidebar_changed, |app, collapsed| app
            .update_settings(|s| s.sidebar_collapsed = collapsed));
        on!(on_set_premium_audio, |app, value| {
            app.update_settings(|s| s.premium_audio = value);
            app.state.borrow_mut().playback.prefetched.clear();
            app.warm_premium();
        });
        on!(on_set_autoplay, |app, value| app
            .update_settings(|s| s.autoplay = value));
        on!(on_set_check_updates, |app, value| {
            app.update_settings(|s| s.check_updates = value);
            let ui = app.ui();
            ui.set_check_updates(value);
            ui.set_update_status(up_to_date_status(value).into());
            if value {
                app.check_for_update();
            }
        });
        on!(on_install_update, |app| app.install_update());
        on!(on_player_event, |app, kind, message| app
            .on_player_event(&kind, &message));
        on!(
            on_media_action,
            |app, action, value| match action.as_str() {
                "toggle" => app.toggle_play(),
                "play" => app.set_paused(false),
                "pause" => app.set_paused(true),
                "next" => app.next(true),
                "previous" => app.previous(),
                "seek-by" => app.seek_to(app.player.position() + value as f64),
                "seek-to" => app.seek_to(value as f64),
                _ => {}
            }
        );

        let api = ui.global::<ApiGlobal>();
        macro_rules! on_api {
            ($setter:ident, |$app:ident $(, $arg:ident)*| $body:expr) => {{
                let weak = Rc::downgrade(self);
                api.$setter(move |$($arg),*| {
                    if let Some($app) = weak.upgrade() {
                        $body
                    }
                });
            }};
        }
        on_api!(on_navigate, |app, kind, id, title| {
            let id = id.to_string();
            if kind == "song" {
                app.play_song_card(&id);
                return;
            }
            let route = match kind.as_str() {
                "album" => Route::Album(id),
                "playlist" => Route::Playlist(id),
                "artist" => Route::Artist(id),
                "genre" => Route::Genre(id, title.to_string()),
                _ => return,
            };
            app.navigate(route, true);
        });
        on_api!(on_play_track, |app, section, index| app
            .play_from_section(section as usize, index as usize));
        on_api!(on_track_action, |app, action, section, index| app
            .track_action(&action, section as usize, index as usize));
        on_api!(on_play_card, |app, kind, id| app.play_card(
            &kind,
            id.to_string(),
            false
        ));
        on_api!(on_card_radio, |app, kind, id| app.play_card(
            &kind,
            id.to_string(),
            true
        ));
        on_api!(on_header_action, |app, action| app.header_action(&action));
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

    // Navigation

    fn navigate(self: &Rc<Self>, route: Route, push: bool) {
        let ui = self.ui();
        let generation = {
            let mut state = self.state.borrow_mut();
            if push {
                if state.route == route && state.page_generation > 0 {
                    return;
                }
                let current = HistoryEntry {
                    route: state.route.clone(),
                    scroll: ui.get_page_scroll(),
                };
                state.back.push(current);
                state.forward.clear();
            }
            state.route = route.clone();
            state.page_generation += 1;
            state.page_generation
        };
        self.update_nav(&ui);
        ui.set_page_loading(true);
        ui.set_page_error(SharedString::new());
        match &route {
            Route::Search(query) => ui.set_search_text(query.into()),
            _ => ui.set_search_text(SharedString::new()),
        }

        let signed_in = self.state.borrow().signed_in;
        if route == Route::Library && !signed_in {
            ui.set_page_loading(false);
            ui.set_page_error("Sign in to see your playlists, albums and artists.".into());
            ui.set_sign_in_open(true);
            return;
        }

        let cached = self
            .state
            .borrow()
            .page_cache
            .get(&route)
            .filter(|(_, at)| at.elapsed() < PAGE_CACHE_FOR)
            .map(|(page, _)| page.clone());
        if let Some(page) = cached {
            ui.set_page_loading(false);
            self.show_page(page);
            return;
        }

        let api = self.api.clone();
        self.spawn(
            async move { api.page(&route, signed_in).await.map(|page| (route, page)) },
            move |app, result| {
                if app.state.borrow().page_generation != generation {
                    return;
                }
                let ui = app.ui();
                ui.set_page_loading(false);
                match result {
                    Ok((route, page)) => {
                        app.cache_page(route, page.clone());
                        app.show_page(page);
                    }
                    Err(e) => {
                        log::warn!("page failed: {e}");
                        ui.set_page_error(e.into());
                    }
                }
            },
        );
    }

    fn cache_page(&self, route: Route, page: Page) {
        let mut state = self.state.borrow_mut();
        let cache = &mut state.page_cache;
        cache.retain(|_, (_, at)| at.elapsed() < PAGE_CACHE_FOR);
        if cache.len() >= PAGE_CACHE_SIZE
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(r, _)| r.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(route, (page, Instant::now()));
    }

    fn reload(self: &Rc<Self>) {
        let route = self.state.borrow().route.clone();
        self.state.borrow_mut().page_cache.remove(&route);
        self.navigate(route, false);
    }

    fn go_back(self: &Rc<Self>) {
        let scroll = self.ui().get_page_scroll();
        let entry = {
            let mut state = self.state.borrow_mut();
            let Some(entry) = state.back.pop() else {
                return;
            };
            let current = HistoryEntry {
                route: state.route.clone(),
                scroll,
            };
            state.forward.push(current);
            state.restore_scroll = Some(entry.scroll);
            entry
        };
        self.navigate(entry.route, false);
    }

    fn go_forward(self: &Rc<Self>) {
        let scroll = self.ui().get_page_scroll();
        let entry = {
            let mut state = self.state.borrow_mut();
            let Some(entry) = state.forward.pop() else {
                return;
            };
            let current = HistoryEntry {
                route: state.route.clone(),
                scroll,
            };
            state.back.push(current);
            state.restore_scroll = Some(entry.scroll);
            entry
        };
        self.navigate(entry.route, false);
    }

    fn update_nav(&self, ui: &AppWindow) {
        let state = self.state.borrow();
        ui.set_nav_current(state.route.nav_name().into());
        ui.set_can_back(!state.back.is_empty());
        ui.set_can_forward(!state.forward.is_empty());
    }

    fn show_page(self: &Rc<Self>, page: Page) {
        let (generation, route) = {
            let state = self.state.borrow();
            (state.page_generation, state.route.clone())
        };
        let mut rows = Vec::new();
        // (row, thumbnail) for track artwork to fetch.
        let mut track_art = Vec::new();
        // Each card model with the thumbnails its cards still need.
        let mut card_art: Vec<CardArt> = Vec::new();

        if route == Route::Home {
            rows.push(RowItem {
                kind: RowKind::Greeting,
                title: s(greeting()),
                ..Default::default()
            });
        }

        let header_edge = match &page.header {
            Some(h) if h.banner => images::BANNER,
            _ => images::HEADER,
        };
        if let Some(header) = &page.header {
            rows.push(RowItem {
                kind: RowKind::Header,
                label: s(&header.label),
                title: s(&header.title),
                subtitle: s(&header.subtitle),
                meta: s(&header.meta),
                description: s(&header.description),
                art: header
                    .thumb
                    .as_deref()
                    .and_then(|url| images::cached(url, header_edge))
                    .unwrap_or_default(),
                round: header.round,
                banner: header.banner,
                can_play: !header.tracks.is_empty(),
                can_shuffle: !header.tracks.is_empty() || header.shuffle_playlist.is_some(),
                can_radio: header.radio_id.is_some(),
                ..Default::default()
            });
        }

        for (section_index, section) in page.sections.iter().enumerate() {
            let title = match section {
                Section::Tracks { title, .. } | Section::Cards { title, .. } => title,
            };
            if !title.is_empty() {
                rows.push(RowItem {
                    kind: RowKind::Title,
                    title: s(title),
                    ..Default::default()
                });
            }
            match section {
                Section::Tracks {
                    tracks, numbered, ..
                } => {
                    // Album and playlist track lists get column labels.
                    if title.is_empty() {
                        rows.push(RowItem {
                            kind: RowKind::Columns,
                            show_album: !*numbered,
                            ..Default::default()
                        });
                    }
                    for (i, track) in tracks.iter().enumerate() {
                        if track.thumb.is_some() && !*numbered {
                            track_art.push((rows.len(), track.thumb.clone()));
                        }
                        rows.push(track_row(track, i, *numbered, section_index));
                    }
                }
                Section::Cards { cards, layout, .. } => {
                    let edge = match layout {
                        CardLayout::Top => images::TOP,
                        _ => images::CARD,
                    };
                    let model = Rc::new(VecModel::from(
                        cards.iter().map(|c| card_item(c, edge)).collect::<Vec<_>>(),
                    ));
                    card_art.push((
                        Rc::clone(&model),
                        cards.iter().map(|c| c.thumb.clone()).collect(),
                        edge,
                    ));
                    rows.push(RowItem {
                        kind: match layout {
                            CardLayout::Carousel => RowKind::Carousel,
                            CardLayout::Grid => RowKind::Grid,
                            CardLayout::Chips => RowKind::Chips,
                            CardLayout::Top => RowKind::Top,
                        },
                        cards: ModelRc::from(model),
                        section: section_index as i32,
                        ..Default::default()
                    });
                }
            }
        }

        let ui = self.ui();
        let header_thumb = page.header.as_ref().and_then(|h| h.thumb.clone());
        ui.set_page_title(s(page.header.as_ref().map_or("", |h| h.title.as_str())));
        ui.set_page_can_play(page.header.as_ref().is_some_and(|h| !h.tracks.is_empty()));
        let tint = header_thumb
            .as_deref()
            .and_then(|url| images::accent(url, header_edge))
            .unwrap_or_else(|| route_tint(&route));
        ui.set_accent(tint);

        self.state.borrow_mut().page = page;
        self.rows.set_vec(rows);
        let scroll = self.state.borrow_mut().restore_scroll.take().unwrap_or(0.0);
        ui.set_page_scroll(scroll);

        let accent_url = header_thumb.clone();
        self.load_image(header_thumb, header_edge, move |app, image| {
            if app.state.borrow().page_generation != generation {
                return;
            }
            if let Some(color) = accent_url
                .as_deref()
                .and_then(|url| images::accent(url, header_edge))
            {
                app.ui().set_accent(color);
            }
            if let Some(index) = (0..app.rows.row_count()).find(|&i| {
                app.rows
                    .row_data(i)
                    .is_some_and(|r| r.kind == RowKind::Header)
            }) {
                let mut row = app.rows.row_data(index).unwrap_or_default();
                row.art = image;
                app.rows.set_row_data(index, row);
            }
        });
        for (row_index, url) in track_art {
            self.load_row_art(row_index, url, generation);
        }
        for (model, urls, edge) in card_art {
            for (i, url) in urls.into_iter().enumerate() {
                let model = Rc::clone(&model);
                self.load_image(url, edge, move |_, image| {
                    if let Some(mut card) = model.row_data(i) {
                        card.art = image;
                        model.set_row_data(i, card);
                    }
                });
            }
        }
        if self.state.borrow().page.more.is_some() {
            self.complete_page(route, generation);
        }
    }

    fn load_row_art(self: &Rc<Self>, row_index: usize, url: Option<String>, generation: u64) {
        self.load_image(url, images::ROW, move |app, image| {
            if app.state.borrow().page_generation != generation {
                return;
            }
            if let Some(mut row) = app.rows.row_data(row_index) {
                row.track.art = image;
                app.rows.set_row_data(row_index, row);
            }
        });
    }

    /// Fetches the rest of a long playlist after its first page shows.
    fn complete_page(self: &Rc<Self>, route: Route, generation: u64) {
        let page = self.state.borrow().page.clone();
        let api = self.api.clone();
        self.spawn(async move { api.complete(page).await }, move |app, page| {
            if let Some((cached, _)) = app.state.borrow_mut().page_cache.get_mut(&route) {
                *cached = page.clone();
            }
            if app.state.borrow().page_generation == generation {
                app.append_tracks(page, generation);
            }
        });
    }

    /// Shows the tracks `page` has beyond those of the page on screen.
    fn append_tracks(self: &Rc<Self>, page: Page, generation: u64) {
        let shown = match self.state.borrow().page.sections.first() {
            Some(Section::Tracks { tracks, .. }) => tracks.len(),
            _ => return,
        };
        let Some(Section::Tracks {
            tracks, numbered, ..
        }) = page.sections.first()
        else {
            return;
        };
        let last_row = (0..self.rows.row_count()).rev().find(|&i| {
            self.rows
                .row_data(i)
                .is_some_and(|r| r.kind == RowKind::Track && r.section == 0)
        });
        let Some(last_row) = last_row else { return };
        for (index, track) in tracks.iter().enumerate().skip(shown) {
            let row_index = last_row + 1 + index - shown;
            self.rows
                .insert(row_index, track_row(track, index, *numbered, 0));
            if !*numbered {
                self.load_row_art(row_index, track.thumb.clone(), generation);
            }
        }
        if let Some(header) = &page.header
            && let Some(i) = (0..self.rows.row_count()).find(|&i| {
                self.rows
                    .row_data(i)
                    .is_some_and(|r| r.kind == RowKind::Header)
            })
            && let Some(mut row) = self.rows.row_data(i)
        {
            row.meta = s(&header.meta);
            self.rows.set_row_data(i, row);
        }
        self.state.borrow_mut().page = page;
    }

    fn suggest(self: &Rc<Self>, query: String) {
        let generation = {
            let mut state = self.state.borrow_mut();
            state.suggest_generation += 1;
            state.suggest_generation
        };
        if query.trim().chars().count() < 2 {
            self.suggest_timer.stop();
            self.ui().set_suggestions(ModelRc::default());
            return;
        }
        // Restarting the timer drops the previous keystroke's request.
        let weak = Rc::downgrade(self);
        self.suggest_timer
            .start(TimerMode::SingleShot, SUGGEST_DELAY, move || {
                let Some(app) = weak.upgrade() else { return };
                let api = app.api.clone();
                let query = query.clone();
                app.spawn(
                    async move { api.suggestions(&query).await },
                    move |app, terms| {
                        if app.state.borrow().suggest_generation != generation {
                            return;
                        }
                        let terms: Vec<SharedString> = terms.iter().take(8).map(s).collect();
                        app.ui()
                            .set_suggestions(ModelRc::from(Rc::new(VecModel::from(terms))));
                    },
                );
            });
    }

    // Starting playback

    fn section_tracks(&self, section: usize) -> Option<(Vec<Track>, QueueMode)> {
        match self.state.borrow().page.sections.get(section)? {
            Section::Tracks { tracks, mode, .. } => Some((tracks.clone(), *mode)),
            Section::Cards { .. } => None,
        }
    }

    fn play_from_section(self: &Rc<Self>, section: usize, index: usize) {
        let Some((tracks, mode)) = self.section_tracks(section) else {
            return;
        };
        let Some(track) = tracks.get(index).cloned() else {
            return;
        };
        match mode {
            QueueMode::Section => {
                let shuffle = self.state.borrow().settings.shuffle;
                self.state.borrow_mut().queue.set(tracks, index, shuffle);
                self.start_current();
            }
            QueueMode::Radio => self.play_with_radio(track),
        }
    }

    /// Plays `track` now and queues a radio based on it, like YouTube Music.
    fn play_with_radio(self: &Rc<Self>, track: Track) {
        let id = track.id.clone();
        {
            let mut state = self.state.borrow_mut();
            state.queue.set(vec![track], 0, false);
            // Autoplay would otherwise fetch the same radio once this starts.
            state.playback.extending = true;
        }
        self.start_current();
        let api = self.api.clone();
        let seed = id.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            move |app, result| {
                app.state.borrow_mut().playback.extending = false;
                match result {
                    Ok(tracks) => {
                        let still_first =
                            app.state.borrow().queue.get(0).is_some_and(|t| t.id == id);
                        if still_first {
                            app.append_unique(tracks);
                        }
                    }
                    Err(e) => {
                        log::warn!("radio failed: {e}");
                        app.extend_with_radio_if_ending();
                    }
                }
            },
        );
    }

    fn append_unique(self: &Rc<Self>, tracks: Vec<Track>) {
        {
            let mut state = self.state.borrow_mut();
            let fresh: Vec<Track> = {
                let known: HashSet<&str> = state.queue.items().map(|t| t.id.as_str()).collect();
                tracks
                    .into_iter()
                    .filter(|t| !known.contains(t.id.as_str()))
                    .collect()
            };
            let shuffle = state.settings.shuffle;
            state.queue.extend(fresh);
            // Keep newly added tracks in the shuffle.
            if shuffle {
                state.queue.set_shuffle(false);
                state.queue.set_shuffle(true);
            }
        }
        self.refresh_queue();
        self.prefetch_next();
    }

    fn play_tracks(self: &Rc<Self>, tracks: Vec<Track>, shuffle: bool) {
        if tracks.is_empty() {
            self.toast("Nothing to play here");
            return;
        }
        let start = if shuffle {
            self.state.borrow_mut().queue.random_below(tracks.len())
        } else {
            0
        };
        self.update_settings(|s| s.shuffle = shuffle);
        self.state.borrow_mut().queue.set(tracks, start, shuffle);
        self.start_current();
    }

    fn play_radio(self: &Rc<Self>, radio_id: String) {
        let api = self.api.clone();
        self.spawn(
            async move { api.radio(&radio_id).await },
            |app, result| match result {
                Ok(tracks) => app.play_tracks(tracks, false),
                Err(e) => app.toast(format!("Couldn't start the radio: {e}")),
            },
        );
    }

    fn header_action(self: &Rc<Self>, action: &str) {
        let Some(header) = self.state.borrow().page.header.clone() else {
            return;
        };
        match action {
            "play" => self.play_tracks(header.tracks, false),
            "shuffle" => match header.shuffle_playlist {
                Some(playlist) => {
                    let api = self.api.clone();
                    self.spawn(
                        async move { api.playlist_tracks(&playlist).await },
                        move |app, result| match result {
                            Ok(tracks) if !tracks.is_empty() => app.play_tracks(tracks, true),
                            _ => app.play_tracks(header.tracks, true),
                        },
                    );
                }
                None => self.play_tracks(header.tracks, true),
            },
            "radio" => {
                if let Some(radio) = header.radio_id {
                    self.play_radio(radio);
                }
            }
            _ => {}
        }
    }

    /// Plays a song card from the current page, with a radio after it.
    fn play_song_card(self: &Rc<Self>, id: &str) {
        let track = self
            .state
            .borrow()
            .page
            .sections
            .iter()
            .find_map(|section| match section {
                Section::Cards { cards, .. } => cards
                    .iter()
                    .find(|c| c.id == id)
                    .and_then(|c| c.track.clone()),
                Section::Tracks { .. } => None,
            });
        if let Some(track) = track {
            self.play_with_radio(track);
        }
    }

    fn play_card(self: &Rc<Self>, kind: &str, id: String, radio: bool) {
        if kind == "song" {
            self.play_song_card(&id);
            return;
        }
        let kind = match kind {
            "album" => CardKind::Album,
            "playlist" => CardKind::Playlist,
            "artist" => CardKind::Artist,
            _ => return,
        };
        if radio && kind == CardKind::Playlist {
            self.play_radio(format!("RDAMPL{id}"));
            return;
        }
        let api = self.api.clone();
        self.spawn(
            async move { api.card_tracks(kind, &id).await },
            move |app, result| match result {
                Ok(tracks) if radio => match tracks.into_iter().next() {
                    Some(first) => app.play_with_radio(first),
                    None => app.toast("Nothing to play here"),
                },
                Ok(tracks) => app.play_tracks(tracks, false),
                Err(e) => app.toast(format!("Couldn't load: {e}")),
            },
        );
    }

    fn track_action(self: &Rc<Self>, action: &str, section: usize, index: usize) {
        let Some(track) = self
            .section_tracks(section)
            .and_then(|(tracks, _)| tracks.get(index).cloned())
        else {
            return;
        };
        match action {
            "next" | "queue" => {
                let empty = self.state.borrow().queue.is_empty();
                {
                    let mut state = self.state.borrow_mut();
                    if action == "next" {
                        state.queue.play_next(track.clone());
                    } else {
                        state.queue.push(track.clone());
                    }
                }
                if empty {
                    self.start_current();
                } else {
                    self.refresh_queue();
                    self.toast(if action == "next" {
                        format!("\u{201c}{}\u{201d} plays next", track.title)
                    } else {
                        format!("Added \u{201c}{}\u{201d} to the queue", track.title)
                    });
                }
            }
            "radio" => self.play_with_radio(track),
            "artist" => {
                if let Some(id) = track.first_artist_id() {
                    self.navigate(Route::Artist(id.to_owned()), true);
                }
            }
            "album" => {
                if let Some((id, _)) = track.album {
                    self.navigate(Route::Album(id), true);
                }
            }
            _ => {}
        }
    }

    // Playback

    /// Loads and plays the queue's current track.
    fn start_current(self: &Rc<Self>) {
        let Some(track) = self.state.borrow().queue.current().cloned() else {
            self.stop();
            return;
        };
        let generation = {
            let mut state = self.state.borrow_mut();
            let playback = &mut state.playback;
            playback.generation += 1;
            playback.started = false;
            playback.duration = track.duration.unwrap_or(0) as f64;
            playback.requested_at = Some(Instant::now());
            playback.queued = None;
            playback.queue_tried = None;
            playback.generation
        };
        self.player.stop();
        self.player.set_paused(false);

        let resume = match &self.state.borrow().playback.resume {
            Some((id, position)) if *id == track.id => *position,
            _ => 0.0,
        };
        self.present(&track, resume);
        let ui = self.ui();
        ui.set_np_loading(true);
        ui.set_np_quality(SharedString::new());
        ui.set_np_premium(false);

        let prefetched = self.take_prefetched(&track.id);
        let resolve = self.resolve_future(track.id.clone(), prefetched);
        self.spawn(resolve, move |app, result| {
            if app.state.borrow().playback.generation != generation {
                return;
            }
            match result {
                Ok(stream) => app.load_stream(stream),
                Err(e) => app.playback_failed(&track.title, &e),
            }
        });
    }

    /// A stream resolved ahead of time for `id`, if still fresh.
    fn take_prefetched(&self, id: &str) -> Option<ResolvedStream> {
        self.state
            .borrow_mut()
            .playback
            .prefetched
            .remove(id)
            .filter(|(_, at)| at.elapsed() < PREFETCH_FRESH)
            .map(|(stream, _)| stream)
    }

    /// Shows `track` as the one playing: the player bar, its artwork, the
    /// queue, the media overlay, Discord, the lyrics and the saved session.
    fn present(self: &Rc<Self>, track: &Track, position: f64) {
        let ui = self.ui();
        ui.set_has_track(true);
        ui.set_np_title(s(&track.title));
        ui.set_np_artists(s(track.artist_names()));
        ui.set_np_album(s(track.album.as_ref().map_or("", |(_, n)| n.as_str())));
        ui.set_np_has_artist(track.first_artist_id().is_some());
        ui.set_np_has_album(track.album.is_some());
        ui.set_np_duration(self.state.borrow().playback.duration as f32);
        ui.set_np_position(position as f32);
        ui.set_np_buffered(0.0);
        self.show_playing(true);
        ui.set_np_art(
            track
                .thumb
                .as_deref()
                .and_then(|u| images::cached(u, images::ROW))
                .unwrap_or_default(),
        );
        ui.global::<ApiGlobal>().set_playing_id(s(&track.id));
        let id = track.id.clone();
        let art = track.thumb.clone();
        self.load_image(art.clone(), images::PLAYER, move |app, image| {
            if !app
                .state
                .borrow()
                .queue
                .current()
                .is_some_and(|t| t.id == id)
            {
                return;
            }
            let ui = app.ui();
            ui.set_np_art(image);
            if let Some(color) = art
                .as_deref()
                .and_then(|url| images::accent(url, images::PLAYER))
            {
                ui.set_np_accent(color);
            }
        });

        if let Some(media) = self.media.borrow_mut().as_mut() {
            let large = track.large_thumb();
            media.set_track(
                &track.title,
                &track.artist_names(),
                track.album.as_ref().map_or("", |(_, n)| n.as_str()),
                large.as_deref(),
                track.duration.unwrap_or(0) as f64,
            );
        }
        self.refresh_queue();
        self.save_session();
        self.update_presence();
        self.state.borrow_mut().lyrics_for = None;
        if ui.get_panel_open() && ui.get_panel_tab() == 1 {
            self.load_lyrics();
        }
    }

    /// Resolves a stream: Premium when signed in with Premium, otherwise the
    /// free visionOS stream. Reuses a prefetched one.
    fn resolve_future(
        &self,
        video_id: String,
        prefetched: Option<ResolvedStream>,
    ) -> impl Future<Output = Result<ResolvedStream, String>> + Send + 'static {
        let api = self.api.clone();
        let premium = Arc::clone(&self.premium);
        let want_premium = {
            let state = self.state.borrow();
            state.signed_in
                && state.settings.premium_audio
                && state.playback.force_free.as_deref() != Some(video_id.as_str())
        };
        let dry_run = !want_premium && premium_dry_run();
        async move {
            if let Some(stream) = prefetched {
                return Ok(stream);
            }
            let started = Instant::now();
            let visitor = api.visitor_data(false).await?;
            let cookie = premium.cookie.read().unwrap().clone();
            let not_premium =
                premium.status.load(Ordering::Relaxed) == PREMIUM_NO || premium.paused();
            if let Some(cookie) = cookie.filter(|_| want_premium && !not_premium) {
                match premium_player(&premium).await {
                    Ok(player) => {
                        match premium::resolve(&video_id, Some(&cookie), &visitor, &player).await {
                            Ok(premium::Outcome::Premium(stream)) => {
                                premium.status.store(PREMIUM_YES, Ordering::Relaxed);
                                premium.misses.store(0, Ordering::Relaxed);
                                premium.errors.store(0, Ordering::Relaxed);
                                log::info!(
                                    "{video_id}: Premium stream resolved in {} ms",
                                    started.elapsed().as_millis()
                                );
                                return Ok(stream);
                            }
                            Ok(premium::Outcome::NoPremiumFormats) => {
                                // Only an account that never got Premium formats, over
                                // several songs in a row, is taken as not Premium.
                                let misses = premium.misses.fetch_add(1, Ordering::Relaxed) + 1;
                                if misses >= NOT_PREMIUM_AFTER
                                    && premium.status.load(Ordering::Relaxed) != PREMIUM_YES
                                {
                                    premium.status.store(PREMIUM_NO, Ordering::Relaxed);
                                }
                            }
                            Err(e) => {
                                premium::diag(format!(
                                    "{video_id}: Premium path failed, using the free stream: {e}"
                                ));
                                premium.record_error();
                            }
                        }
                    }
                    Err(e) => premium::diag(format!("could not prepare the player: {e}")),
                }
            } else if dry_run && let Ok(player) = premium_player(&premium).await {
                // Times the Premium path without an account; the result is
                // discarded as YouTube won't serve it anonymously.
                let dry = Instant::now();
                let result = premium::resolve(&video_id, None, &visitor, &player).await;
                premium::diag(format!(
                    "{video_id}: dry run {} in {} ms",
                    if result.is_ok() { "resolved" } else { "failed" },
                    dry.elapsed().as_millis()
                ));
            }
            let free = Instant::now();
            let result = match streams::resolve(&video_id, &visitor).await {
                Ok(stream) => Ok(stream),
                // YouTube sometimes bot-checks a visitor ID; retry with a fresh one.
                Err(e) => {
                    log::debug!("retrying with a new visitor ID: {e}");
                    let visitor = api.visitor_data(true).await?;
                    streams::resolve(&video_id, &visitor).await
                }
            };
            log::info!(
                "{video_id}: free stream resolved in {} ms",
                free.elapsed().as_millis()
            );
            result
        }
    }

    fn load_stream(self: &Rc<Self>, stream: ResolvedStream) {
        self.show_stream(&stream);
        let start_at = self.take_resume_position();
        self.player.load(stream_spec(stream, start_at));
    }

    /// Shows the stream's quality, and fills in a length the track lacked.
    fn show_stream(&self, stream: &ResolvedStream) {
        let ui = self.ui();
        ui.set_np_premium(stream.premium);
        ui.set_np_quality(quality_label(stream).into());
        if let Some(duration) = stream.duration {
            let mut state = self.state.borrow_mut();
            if state.playback.duration <= 0.0 {
                state.playback.duration = duration;
                drop(state);
                ui.set_np_duration(duration as f32);
            }
        }
        self.state.borrow_mut().playback.current_premium = stream.premium;
    }

    /// Queues the next track in the player shortly before the current one
    /// ends, so it follows without a gap, and drops a queued track the
    /// queue no longer has next. Runs every tick, so it copies nothing
    /// until a track is due.
    fn update_gapless(self: &Rc<Self>, position: f64) {
        let (stale, due, generation) = {
            let state = self.state.borrow();
            let playback = &state.playback;
            let next = state.queue.peek_next().map(|t| t.id.as_str());
            let stale = playback
                .queued
                .as_ref()
                .is_some_and(|(id, _)| next != Some(id.as_str()));
            let near_end = playback.started
                && playback.duration > 0.0
                && playback.duration - position <= GAPLESS_LEAD_SECS
                && (playback.queued.is_none() || stale);
            let due = next
                .filter(|id| near_end && playback.queue_tried.as_deref() != Some(*id))
                .map(str::to_owned);
            (stale, due, playback.generation)
        };
        if stale {
            self.state.borrow_mut().playback.queued = None;
            self.player.clear_next();
        }
        let Some(next_id) = due else {
            return;
        };
        self.state.borrow_mut().playback.queue_tried = Some(next_id.clone());
        let prefetched = self.take_prefetched(&next_id);
        let resolve = self.resolve_future(next_id.clone(), prefetched);
        self.spawn(resolve, move |app, result| {
            let still_next = {
                let state = app.state.borrow();
                state.playback.generation == generation
                    && state.queue.peek_next().is_some_and(|t| t.id == next_id)
            };
            match result {
                Ok(stream) if still_next => {
                    app.player.queue_next(stream_spec(stream.clone(), 0.0));
                    app.state.borrow_mut().playback.queued = Some((next_id, stream));
                }
                Ok(_) => {}
                Err(e) => log::debug!("could not queue the next track: {e}"),
            }
        });
    }

    /// The queued track became audible: move the queue on and show it,
    /// without loading anything.
    fn advance_gapless(self: &Rc<Self>) {
        let Some((id, stream)) = self.state.borrow_mut().playback.queued.take() else {
            return;
        };
        let current = self.state.borrow_mut().queue.advance(false).cloned();
        let Some(track) = current.filter(|t| t.id == id) else {
            // The queue changed under the player; play what it says now.
            self.start_current();
            return;
        };
        {
            let mut state = self.state.borrow_mut();
            let playback = &mut state.playback;
            playback.generation += 1;
            playback.started = true;
            playback.failures = 0;
            playback.duration = track.duration.map_or(0.0, f64::from);
            playback.requested_at = None;
            playback.queue_tried = None;
            playback.resume = None;
        }
        self.present(&track, 0.0);
        self.show_stream(&stream);
        self.ui().set_np_loading(false);
        self.update_media_state();
        self.extend_with_radio_if_ending();
        self.prefetch_next();
    }

    fn playback_failed(self: &Rc<Self>, title: &str, error: &str) {
        log::warn!("playback failed: {error}");
        // A Premium stream that will not play gets one retry on the free one.
        let retry_free = {
            let mut state = self.state.borrow_mut();
            let id = state.queue.current().map(|t| t.id.clone());
            if state.playback.current_premium && state.playback.force_free != id {
                state.playback.current_premium = false;
                state.playback.force_free.clone_from(&id);
                state.playback.prefetched.clear();
                id
            } else {
                None
            }
        };
        if let Some(id) = retry_free {
            premium::diag(format!(
                "{id}: Premium stream failed to play ({error}); retrying the free stream"
            ));
            self.start_current();
            return;
        }
        let failures = {
            let mut state = self.state.borrow_mut();
            state.playback.failures += 1;
            state.playback.failures
        };
        self.toast(format!("Couldn't play \u{201c}{title}\u{201d}: {error}"));
        let has_next = self.state.borrow().queue.peek_next().is_some();
        if failures < MAX_CONSECUTIVE_FAILURES && has_next {
            self.next(true);
        } else {
            self.ui().set_np_loading(false);
            self.show_playing(false);
        }
    }

    fn on_player_event(self: &Rc<Self>, kind: &str, message: &str) {
        match kind {
            "started" => {
                let (waited, premium_stream) = {
                    let mut state = self.state.borrow_mut();
                    state.playback.started = true;
                    state.playback.failures = 0;
                    let waited = state.playback.requested_at.take().map(|t| t.elapsed());
                    (waited, state.playback.current_premium)
                };
                if let Some(waited) = waited {
                    let line = format!(
                        "first sound after {} ms ({}, {})",
                        waited.as_millis(),
                        if premium_stream { "Premium" } else { "free" },
                        self.ui().get_np_quality()
                    );
                    if premium_stream || premium_dry_run() {
                        premium::diag(&line);
                    } else {
                        log::info!("{line}");
                    }
                }
                self.ui().set_np_loading(false);
                self.update_media_state();
                self.update_presence();
                self.extend_with_radio_if_ending();
                self.prefetch_next();
            }
            "advanced" => self.advance_gapless(),
            "next-dropped" => {
                let mut state = self.state.borrow_mut();
                state.playback.queued = None;
                state.playback.queue_tried = None;
            }
            "ended" => self.next(false),
            "error" => {
                let title = self
                    .state
                    .borrow()
                    .queue
                    .current()
                    .map(|t| t.title.clone())
                    .unwrap_or_default();
                self.playback_failed(&title, message);
            }
            _ => {}
        }
    }

    /// Resolves the next track's stream ahead of time to skip the wait.
    fn prefetch_next(self: &Rc<Self>) {
        let next = {
            let state = self.state.borrow();
            let Some(next) = state.queue.peek_next().cloned() else {
                return;
            };
            let fresh = state
                .playback
                .prefetched
                .get(&next.id)
                .is_some_and(|(_, at)| at.elapsed() < PREFETCH_FRESH);
            if fresh || state.playback.prefetching.contains(&next.id) {
                return;
            }
            next
        };
        self.state
            .borrow_mut()
            .playback
            .prefetching
            .insert(next.id.clone());
        let resolve = self.resolve_future(next.id.clone(), None);
        self.spawn(resolve, move |app, result| {
            let mut state = app.state.borrow_mut();
            state.playback.prefetching.remove(&next.id);
            match result {
                Ok(stream) => {
                    state
                        .playback
                        .prefetched
                        .insert(next.id, (stream, Instant::now()));
                }
                Err(e) => log::debug!("prefetch failed: {e}"),
            }
        });
    }

    /// Queues a radio when the last track starts, so playback carries on.
    fn extend_with_radio_if_ending(self: &Rc<Self>) {
        let seed = {
            let state = self.state.borrow();
            if !state.settings.autoplay
                || state.queue.repeat != Repeat::Off
                || state.queue.peek_next().is_some()
                || state.playback.extending
            {
                return;
            }
            let Some(current) = state.queue.current() else {
                return;
            };
            current.id.clone()
        };
        self.state.borrow_mut().playback.extending = true;
        let api = self.api.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            |app, result| {
                app.state.borrow_mut().playback.extending = false;
                match result {
                    Ok(tracks) => app.append_unique(tracks),
                    Err(e) => log::warn!("autoplay radio failed: {e}"),
                }
            },
        );
    }

    fn next(self: &Rc<Self>, user: bool) {
        let moved = self.state.borrow_mut().queue.advance(user).is_some();
        if moved {
            self.start_current();
            return;
        }
        if self.state.borrow().settings.autoplay && !self.state.borrow().queue.is_empty() {
            // The radio may still be loading; queue it and move on.
            let seed = self.state.borrow().queue.current().map(|t| t.id.clone());
            if let Some(seed) = seed {
                let api = self.api.clone();
                self.spawn(
                    async move { api.track_radio(&seed).await },
                    |app, result| {
                        if let Ok(tracks) = result {
                            app.append_unique(tracks);
                            if app.state.borrow_mut().queue.advance(true).is_some() {
                                app.start_current();
                                return;
                            }
                        }
                        app.stop();
                    },
                );
                return;
            }
        }
        if !user {
            self.stop();
        }
    }

    fn previous(self: &Rc<Self>) {
        if self.player.position() > RESTART_AFTER_SECS {
            self.seek_to(0.0);
            return;
        }
        if self.state.borrow_mut().queue.back().is_some() {
            self.start_current();
        }
    }

    fn stop(self: &Rc<Self>) {
        self.state.borrow_mut().playback.started = false;
        self.player.stop();
        self.player.set_paused(true);
        let ui = self.ui();
        self.show_playing(false);
        ui.set_np_loading(false);
        self.update_media_state();
        self.update_presence();
    }

    fn toggle_play(self: &Rc<Self>) {
        if !self.ui().get_has_track() {
            return;
        }
        let ended = !self.state.borrow().playback.started && !self.ui().get_np_loading();
        if ended {
            // Replay the current track after it stopped at the queue's end.
            self.start_current();
            return;
        }
        self.set_paused(!self.player.is_paused());
    }

    fn set_paused(self: &Rc<Self>, paused: bool) {
        self.player.set_paused(paused);
        self.show_playing(!paused);
        self.update_media_state();
        self.update_presence();
    }

    fn seek_to(self: &Rc<Self>, secs: f64) {
        let duration = self.state.borrow().playback.duration;
        let secs = if duration > 0.0 {
            secs.clamp(0.0, duration - 0.5)
        } else {
            secs.max(0.0)
        };
        self.player.seek(secs);
        self.ui().set_np_position(secs as f32);
        self.update_media_state();
        self.update_presence();
    }

    fn set_volume(&self, volume: f32) {
        self.player.set_volume(volume);
        let mut state = self.state.borrow_mut();
        state.settings.volume = volume;
        state.settings.muted = false;
        drop(state);
        let ui = self.ui();
        ui.set_volume(volume);
        ui.set_muted(false);
    }

    fn toggle_mute(&self) {
        let (muted, volume) = {
            let mut state = self.state.borrow_mut();
            state.settings.muted = !state.settings.muted;
            (state.settings.muted, state.settings.volume)
        };
        self.player.set_volume(if muted { 0.0 } else { volume });
        self.ui().set_muted(muted);
    }

    fn toggle_shuffle(self: &Rc<Self>) {
        let on = !self.state.borrow().settings.shuffle;
        self.state.borrow_mut().queue.set_shuffle(on);
        self.update_settings(|s| s.shuffle = on);
        self.refresh_queue();
        self.prefetch_next();
    }

    fn cycle_repeat(self: &Rc<Self>) {
        let repeat = {
            let mut state = self.state.borrow_mut();
            state.queue.repeat = state.queue.repeat.next();
            state.queue.repeat
        };
        self.update_settings(|s| s.repeat = repeat.index());
        self.prefetch_next();
    }

    fn on_tick(self: &Rc<Self>) {
        let ui = self.ui();
        let status = self.premium.status.load(Ordering::Relaxed);
        if self.premium_shown.replace(status) != status {
            ui.set_premium_status(
                match status {
                    PREMIUM_YES => "Active: playing 256 kbps Premium streams.",
                    PREMIUM_NO => "This account has no Premium; playing standard streams.",
                    _ => "When signed in with YouTube Premium, plays 256 kbps Opus.",
                }
                .into(),
            );
        }
        if !ui.get_has_track() {
            return;
        }
        let started = self.state.borrow().playback.started;
        if started {
            let position = self.player.position();
            ui.set_np_position(position as f32);
            let buffered = self.player.buffered();
            ui.set_np_buffered(buffered);
            let stalled = self.player.is_buffering() && !self.player.is_paused();
            ui.set_np_loading(stalled && buffered < 1.0);
            self.update_gapless(position);
        }
    }

    fn update_media_state(&self) {
        let playing = if self.ui().get_has_track() {
            Some(!self.player.is_paused())
        } else {
            None
        };
        if let Some(media) = self.media.borrow_mut().as_mut() {
            media.set_state(playing, self.player.position());
        }
    }

    fn update_presence(&self) {
        let state = self.state.borrow();
        let track = match state.queue.current() {
            Some(track) if state.settings.discord && self.ui().get_has_track() => {
                Some(presence::Track {
                    video_id: track.id.clone(),
                    title: track.title.clone(),
                    artist: track.artist_names(),
                    album: track.album.as_ref().map(|(_, name)| name.clone()),
                    artwork_url: track.large_thumb(),
                    duration: state.playback.duration,
                    position: self.player.position().min(state.playback.duration.max(0.0)),
                    paused: self.player.is_paused() || !state.playback.started,
                })
            }
            _ => None,
        };
        self.presence.update(track);
    }

    fn refresh_queue(self: &Rc<Self>) {
        let (revision, current) = {
            let state = self.state.borrow();
            (state.queue.revision(), state.queue.current_index())
        };
        let current = current.map_or(-1, |i| i as i32);
        let ui = self.ui();
        let previous = ui.get_queue_current();
        ui.set_queue_current(current);
        if self.queue_shown.get() == Some(revision) {
            // Same entries: only the highlight moves.
            for (index, is_current) in [(previous, false), (current, true)] {
                if let Ok(i) = usize::try_from(index)
                    && let Some(mut item) = self.queue_model.row_data(i)
                    && item.current != is_current
                {
                    item.current = is_current;
                    self.queue_model.set_row_data(i, item);
                }
            }
            return;
        }
        self.queue_shown.set(Some(revision));

        let mut missing = Vec::new();
        let items: Vec<QueueItem> = self
            .state
            .borrow()
            .queue
            .items()
            .enumerate()
            .map(|(i, t)| {
                let art = t
                    .thumb
                    .as_deref()
                    .and_then(|u| images::cached(u, images::ROW));
                if art.is_none() && t.thumb.is_some() {
                    missing.push((i, t.thumb.clone()));
                }
                QueueItem {
                    title: s(&t.title),
                    artists: s(t.artist_names()),
                    duration: s(t.duration.map(api::format_duration).unwrap_or_default()),
                    art: art.unwrap_or_default(),
                    current: i as i32 == current,
                }
            })
            .collect();
        self.queue_model.set_vec(items);
        for (i, url) in missing {
            self.load_image(url, images::ROW, move |app, image| {
                if app.queue_shown.get() != Some(revision) {
                    return;
                }
                if let Some(mut item) = app.queue_model.row_data(i) {
                    item.art = image;
                    app.queue_model.set_row_data(i, item);
                }
            });
        }
    }

    fn load_lyrics(self: &Rc<Self>) {
        let Some(id) = self.state.borrow().queue.current().map(|t| t.id.clone()) else {
            self.ui()
                .set_lyrics("Play something to see its lyrics.".into());
            return;
        };
        if self.state.borrow().lyrics_for.as_deref() == Some(id.as_str()) {
            return;
        }
        self.state.borrow_mut().lyrics_for = Some(id.clone());
        let ui = self.ui();
        ui.set_lyrics_loading(true);
        let api = self.api.clone();
        let requested = id.clone();
        self.spawn(
            async move { api.lyrics(&requested).await },
            move |app, result| {
                if app.state.borrow().lyrics_for.as_deref() != Some(id.as_str()) {
                    return;
                }
                let text = match result {
                    Ok(Some(text)) => text,
                    Ok(None) => "Lyrics aren't available for this song.".to_owned(),
                    Err(e) => {
                        app.state.borrow_mut().lyrics_for = None;
                        format!("Couldn't load lyrics: {e}")
                    }
                };
                let ui = app.ui();
                ui.set_lyrics(text.into());
                ui.set_lyrics_loading(false);
            },
        );
    }

    // Account

    fn sign_in(self: &Rc<Self>, cookie: String) {
        if cookie.trim().is_empty() {
            self.ui()
                .set_sign_in_error("Paste your cookies first.".into());
            return;
        }
        let ui = self.ui();
        ui.set_signing_in(true);
        ui.set_sign_in_error(SharedString::new());
        let api = self.api.clone();
        let header = premium::cookie_header(&cookie);
        self.spawn(
            async move { api.sign_in(&cookie).await },
            move |app, result| {
                let ui = app.ui();
                ui.set_signing_in(false);
                match result {
                    Ok(()) => {
                        app.state.borrow_mut().signed_in = true;
                        app.state.borrow_mut().page_cache.clear();
                        app.load_library();
                        app.load_account_photo();
                        settings::save_cookie(&header);
                        *app.premium.cookie.write().unwrap() = Some(header);
                        app.premium.status.store(PREMIUM_UNKNOWN, Ordering::Relaxed);
                        app.premium.misses.store(0, Ordering::Relaxed);
                        app.warm_premium();
                        ui.set_signed_in(true);
                        ui.set_sign_in_open(false);
                        app.toast("Signed in");
                        app.show_fresh(Route::Library);
                    }
                    Err(e) => ui.set_sign_in_error(format!("Sign-in failed: {e}").into()),
                }
            },
        );
    }

    fn sign_in_with_google(self: &Rc<Self>) {
        if let Some(open) = self.google_sign_in.borrow().as_ref() {
            let _ = open.window().show();
            return;
        }
        // Each step is deferred: WebView2 calls back from inside its own
        // events, and the window must not be dropped from its own handler.
        let weak = Rc::downgrade(self);
        let on_page = move || {
            let weak = weak.clone();
            Timer::single_shot(Duration::ZERO, move || {
                if let Some(app) = weak.upgrade() {
                    app.check_google_sign_in();
                }
            });
        };
        let open = match GoogleSignIn::open() {
            Ok(open) => open,
            Err(e) => return self.google_sign_in_failed(&e),
        };
        let weak = Rc::downgrade(self);
        open.window().on_resized(move || {
            if let Some(app) = weak.upgrade()
                && let Some(open) = app.google_sign_in.borrow().as_ref()
            {
                open.fit();
            }
        });
        let weak = Rc::downgrade(self);
        open.window().window().on_close_requested(move || {
            let weak = weak.clone();
            Timer::single_shot(Duration::ZERO, move || {
                if let Some(app) = weak.upgrade() {
                    app.close_google_sign_in();
                }
            });
            slint::CloseRequestResponse::HideWindow
        });
        *self.google_sign_in.borrow_mut() = Some(open);
        let ui = self.ui();
        ui.set_sign_in_error(SharedString::new());
        ui.set_signing_in_browser(true);

        // Windows creates the window a moment after it is shown. The page is
        // attached with the window out of its cell, as WebView2 pumps
        // messages meanwhile, which may call back into the app.
        let weak = Rc::downgrade(self);
        let attempts = std::cell::Cell::new(0);
        self.sign_in_timer
            .start(TimerMode::Repeated, Duration::from_millis(50), move || {
                let Some(app) = weak.upgrade() else { return };
                let Some(mut open) = app.google_sign_in.borrow_mut().take() else {
                    app.sign_in_timer.stop();
                    return;
                };
                let result = open.attach(on_page.clone());
                *app.google_sign_in.borrow_mut() = Some(open);
                attempts.set(attempts.get() + 1);
                match result {
                    Ok(false) if attempts.get() < 100 => return,
                    Ok(false) => app.google_sign_in_failed("the sign-in window did not open"),
                    Ok(true) => {}
                    Err(e) => app.google_sign_in_failed(&e),
                }
                app.sign_in_timer.stop();
            });
    }

    fn google_sign_in_failed(&self, error: &str) {
        log::warn!("{error}");
        self.close_google_sign_in();
        self.ui().set_sign_in_error(
            format!("Couldn't sign in with Google: {error}. You can paste cookies instead.").into(),
        );
    }

    fn check_google_sign_in(self: &Rc<Self>) {
        let cookie = self
            .google_sign_in
            .borrow()
            .as_ref()
            .and_then(GoogleSignIn::cookie);
        if let Some(cookie) = cookie {
            self.close_google_sign_in();
            self.sign_in(cookie);
        }
    }

    fn close_google_sign_in(&self) {
        if let Some(open) = self.google_sign_in.borrow_mut().take() {
            let _ = open.window().hide();
        }
        self.ui().set_signing_in_browser(false);
    }

    // Updates

    /// Looks for a newer release, when checks are on and none is waiting.
    fn check_for_update(self: &Rc<Self>) {
        if !self.state.borrow().settings.check_updates || self.update.borrow().is_some() {
            return;
        }
        self.spawn(update::check(), |app, result| match result {
            Ok(Some(found)) => {
                let ui = app.ui();
                ui.set_update_version(s(&found.version));
                ui.set_update_status(
                    format!(
                        "Version {} is available. The app restarts to install it.",
                        found.version
                    )
                    .into(),
                );
                app.toast(format!(
                    "ytm-native {} is available; update it from Settings",
                    found.version
                ));
                *app.update.borrow_mut() = Some(found);
            }
            Ok(None) => {}
            Err(e) => log::debug!("{e}"),
        });
    }

    /// Downloads and checks the update, then closes the app as its close
    /// button does; the update takes over as the app exits.
    fn install_update(self: &Rc<Self>) {
        let Some(found) = self.update.borrow().clone() else {
            return;
        };
        let ui = self.ui();
        ui.set_update_busy(true);
        ui.set_update_status(format!("Downloading version {}…", found.version).into());
        let weak = self.ui.clone();
        let shown = Arc::new(AtomicU8::new(0));
        let progress = move |fraction: f32| {
            let percent = (fraction * 100.0) as u8;
            if shown.swap(percent, Ordering::Relaxed) != percent {
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_update_status(format!("Downloading… {percent}%").into());
                });
            }
        };
        let download = async move {
            let result = update::download(&found, progress).await;
            (found, result)
        };
        self.spawn(download, |app, (found, result)| {
            let ui = app.ui();
            match result.and_then(|path| update::stage(&found, path)) {
                Ok(()) => {
                    ui.set_update_status("Restarting to finish the update…".into());
                    let close = slint::platform::WindowEvent::CloseRequested;
                    if let Err(e) = ui.window().dispatch_event_with_result(close) {
                        log::warn!("could not close for the update: {e}");
                    }
                }
                Err(e) => {
                    ui.set_update_busy(false);
                    ui.set_update_status(format!("The update failed: {e}").into());
                }
            }
        });
    }

    /// Prepares the Premium player script in the background, so the first
    /// song does not wait for it. Only runs when Premium audio can apply.
    fn warm_premium(self: &Rc<Self>) {
        let wanted = {
            let state = self.state.borrow();
            state.signed_in
                && state.settings.premium_audio
                && self.premium.cookie.read().unwrap().is_some()
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

    /// Fills the sidebar with the signed-in user's playlists.
    fn load_library(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(
            async move { api.library_playlists().await },
            |app, result| match result {
                Ok(cards) => {
                    let items: Vec<CardItem> =
                        cards.iter().map(|c| card_item(c, images::ROW)).collect();
                    app.library.set_vec(items);
                    for (i, url) in cards.into_iter().map(|c| c.thumb).enumerate() {
                        app.load_image(url, images::ROW, move |app, image| {
                            if let Some(mut item) = app.library.row_data(i) {
                                item.art = image;
                                app.library.set_row_data(i, item);
                            }
                        });
                    }
                }
                Err(e) => log::warn!("{e}"),
            },
        );
    }

    /// Drops back to signed out when YouTube rejects the stored cookie.
    fn sign_in_expired(self: &Rc<Self>) {
        log::warn!("the stored sign-in was rejected");
        let on_home = {
            let mut state = self.state.borrow_mut();
            state.signed_in = false;
            state.page_cache.clear();
            state.route == Route::Home
        };
        self.library.set_vec(Vec::new());
        let ui = self.ui();
        ui.set_signed_in(false);
        ui.set_account_photo(Image::default());
        if on_home {
            self.reload();
        }
    }

    /// Shows `route` loaded afresh, as signing in or out changes what it
    /// holds. Navigating to the page already shown would do nothing.
    fn show_fresh(self: &Rc<Self>, route: Route) {
        if self.state.borrow().route == route {
            self.reload();
        } else {
            self.navigate(route, true);
        }
    }

    /// Puts the account's profile picture on the account button.
    fn load_account_photo(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(
            async move { api.account_photo().await },
            |app, result| match result {
                Ok(url) => app.load_image(url, images::AVATAR, |app, image| {
                    if app.state.borrow().signed_in {
                        app.ui().set_account_photo(image);
                    }
                }),
                Err(e) => log::warn!("could not load the account's picture: {e}"),
            },
        );
    }

    fn sign_out(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(async move { api.sign_out().await }, |app, ()| {
            app.state.borrow_mut().signed_in = false;
            app.state.borrow_mut().page_cache.clear();
            app.library.set_vec(Vec::new());
            settings::remove_cookie();
            *app.premium.cookie.write().unwrap() = None;
            app.premium.status.store(PREMIUM_UNKNOWN, Ordering::Relaxed);
            app.premium.misses.store(0, Ordering::Relaxed);
            let ui = app.ui();
            ui.set_account_photo(Image::default());
            ui.set_signed_in(false);
            ui.set_settings_open(false);
            app.toast("Signed out");
            app.show_fresh(Route::Home);
        });
    }
}

/// "Good morning" and so on, by the local time.
fn greeting() -> &'static str {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    greeting_for(now.hour())
}

fn greeting_for(hour: u8) -> &'static str {
    match hour {
        4..12 => "Good morning",
        12..18 => "Good afternoon",
        18..23 => "Good evening",
        _ => "Good night",
    }
}

/// The header tint for pages without artwork.
fn route_tint(route: &Route) -> slint::Color {
    let rgb = match route {
        Route::Home => 0x5c0f1c,
        Route::Explore => 0x1e3264,
        Route::Library => 0x3d1f5c,
        Route::Search(_) => 0x2a2a2a,
        _ => 0x333333,
    };
    slint::Color::from_argb_encoded(0xff00_0000 | rgb)
}

const PREMIUM_UNKNOWN: u8 = 0;
const PREMIUM_YES: u8 = 1;
const PREMIUM_NO: u8 = 2;
/// Songs in a row without Premium formats before the account counts as not
/// Premium, as some songs simply have no Premium version.
const NOT_PREMIUM_AFTER: u8 = 3;

/// Premium audio state shared with background tasks.
struct PremiumShared {
    /// The cut-down player script, loaded once per session.
    player: tokio::sync::Mutex<Option<Arc<premium::Player>>>,
    cookie: std::sync::RwLock<Option<String>>,
    status: AtomicU8,
    /// Songs in a row that offered no Premium formats.
    misses: AtomicU8,
    /// Premium requests in a row that failed outright.
    errors: AtomicU8,
    /// When preparing the player last failed, to avoid refetching it per song.
    load_failed: std::sync::Mutex<Option<Instant>>,
    /// The Premium path is skipped until then, after repeated errors.
    paused_until: std::sync::Mutex<Option<Instant>>,
}

/// Cool-off after the player fails to prepare or Premium keeps failing.
const PREMIUM_BACKOFF: Duration = Duration::from_secs(10 * 60);
const PREMIUM_MAX_ERRORS: u8 = 3;

impl PremiumShared {
    fn paused(&self) -> bool {
        self.paused_until
            .lock()
            .unwrap()
            .is_some_and(|until| Instant::now() < until)
    }

    fn record_error(&self) {
        if self.errors.fetch_add(1, Ordering::Relaxed) + 1 >= PREMIUM_MAX_ERRORS {
            self.errors.store(0, Ordering::Relaxed);
            *self.paused_until.lock().unwrap() = Some(Instant::now() + PREMIUM_BACKOFF);
            premium::diag("Premium requests keep failing; using free streams for 10 minutes");
        }
    }
}

/// `YTM_PREMIUM_DRYRUN=1` times the Premium path while playing free streams.
fn premium_dry_run() -> bool {
    std::env::var_os("YTM_PREMIUM_DRYRUN").is_some()
}

async fn premium_player(shared: &PremiumShared) -> Result<Arc<premium::Player>, String> {
    let mut slot = shared.player.lock().await;
    if let Some(player) = slot.as_ref() {
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
    match premium::load_player().await {
        Ok(player) => {
            let player = Arc::new(player);
            *slot = Some(Arc::clone(&player));
            Ok(player)
        }
        Err(e) => {
            *shared.load_failed.lock().unwrap() = Some(Instant::now());
            Err(e)
        }
    }
}

/// The player-bar badge text. Premium formats show YouTube's nominal 256
/// kbps; others show the stream's bitrate.
fn up_to_date_status(checking: bool) -> String {
    if checking {
        format!(
            "ytm-native {}. Checks GitHub for new versions.",
            update::CURRENT
        )
    } else {
        format!("ytm-native {}. Automatic checks are off.", update::CURRENT)
    }
}

fn stream_spec(stream: ResolvedStream, start_at: f64) -> StreamSpec {
    StreamSpec {
        url: stream.url,
        size: stream.size,
        mime: stream.mime,
        headers: stream.headers,
        loudness_db: stream.loudness_db,
        start_at,
    }
}

fn quality_label(stream: &ResolvedStream) -> String {
    let codec = stream.codec.to_uppercase();
    match (stream.itag, stream.bitrate_kbps) {
        (774 | 141, _) => format!("{codec} 256"),
        (_, Some(kbps)) => format!("{codec} {}", kbps.round()),
        (_, None) => codec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets_by_hour() {
        assert_eq!(greeting_for(4), "Good morning");
        assert_eq!(greeting_for(11), "Good morning");
        assert_eq!(greeting_for(12), "Good afternoon");
        assert_eq!(greeting_for(18), "Good evening");
        assert_eq!(greeting_for(23), "Good night");
        assert_eq!(greeting_for(2), "Good night");
    }

    #[test]
    fn labels_premium_formats_with_their_nominal_bitrate() {
        let stream = |itag, kbps| ResolvedStream {
            url: String::new(),
            size: 0,
            mime: String::new(),
            headers: Vec::new(),
            codec: "Opus".into(),
            bitrate_kbps: Some(kbps),
            duration: None,
            loudness_db: None,
            premium: itag == 774,
            itag,
        };
        assert_eq!(quality_label(&stream(774, 284.6)), "OPUS 256");
        assert_eq!(quality_label(&stream(251, 146.4)), "OPUS 146");
    }
}
