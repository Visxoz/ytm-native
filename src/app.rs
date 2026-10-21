//! Ties the UI to the API, the queue and the audio engine. Everything here
//! runs on the UI thread; network and disk work is spawned on Tokio and its
//! results are picked up again through `slint::spawn_local`.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};

use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use tokio::{runtime::Handle, sync::watch};

use crate::{
    Api as ApiGlobal, AppWindow, CardItem, QueueItem, RowItem, RowKind, TrackItem,
    api::{self, Api, Card, CardKind, CardLayout, Page, QueueMode, Route, Section, Track},
    audio::{Player, PlayerEvent, StreamSpec},
    images,
    media_controls::{Action, Controls},
    presence::{self, PresenceHandle},
    queue::{Queue, Repeat},
    settings::{self, Settings},
    streams::{self, ResolvedStream},
};

const TICK: Duration = Duration::from_millis(250);
const TOAST_FOR: Duration = Duration::from_secs(5);
/// Pressing previous later than this restarts the track instead.
const RESTART_AFTER_SECS: f64 = 3.0;
/// Stream URLs last about six hours; refresh well before.
const PREFETCH_FRESH: Duration = Duration::from_secs(60 * 60);
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const SUGGEST_DELAY: Duration = Duration::from_millis(150);

type EngineState = Option<Result<(), String>>;
type CardArt = (Rc<VecModel<CardItem>>, Vec<Option<String>>);

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
    queue_generation: u64,
    playback: Playback,
    lyrics_for: Option<String>,
    suggest_generation: u64,
}

pub struct App {
    ui: slint::Weak<AppWindow>,
    rt: Handle,
    api: Api,
    player: Player,
    presence: PresenceHandle,
    media: RefCell<Option<Controls>>,
    engine: watch::Receiver<EngineState>,
    state: RefCell<State>,
    rows: Rc<VecModel<RowItem>>,
    queue_model: Rc<VecModel<QueueItem>>,
    tick: Timer,
    toast_timer: Timer,
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
            .and_then(|url| images::cached(url, api::ROW_THUMB))
            .unwrap_or_default(),
        number: s(number.to_string()),
        has_artist: track.first_artist_id().is_some(),
        has_album: track.album.is_some(),
    }
}

fn card_item(card: &Card) -> CardItem {
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
            .and_then(|url| images::cached(url, api::CARD_THUMB))
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
                PlayerEvent::Ended => ("ended", String::new()),
                PlayerEvent::Error(e) => ("error", e),
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.invoke_player_event(kind.into(), message.into())
            });
        });
        player.set_volume(if settings.muted { 0.0 } else { settings.volume });
        player.set_normalise(settings.normalise);

        let (engine_tx, engine) = watch::channel(None);
        rt.spawn_blocking(move || {
            let _ = engine_tx.send(Some(streams::ensure_ytdlp()));
        });

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
            engine,
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
                queue_generation: 0,
                playback: Playback::default(),
                lyrics_for: None,
                suggest_generation: 0,
            }),
            rows: Rc::new(VecModel::default()),
            queue_model: Rc::new(VecModel::default()),
            tick: Timer::default(),
            toast_timer: Timer::default(),
        });
        app.bind(ui);
        app.apply_settings_to_ui(ui);
        ui.set_engine_status("Preparing playback engine…".into());
        ui.set_rows(ModelRc::from(Rc::clone(&app.rows)));
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

    fn toast(self: &Rc<Self>, message: impl Into<String>) {
        let message = message.into();
        log::info!("{message}");
        self.ui().set_toast(message.into());
        let weak = self.ui.clone();
        self.toast_timer
            .start(TimerMode::SingleShot, TOAST_FOR, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_toast(SharedString::new());
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
        ui.set_prefer_aac(settings.prefer_aac);
        ui.set_normalise(settings.normalise);
        ui.set_discord(settings.discord);
        ui.set_autoplay(settings.autoplay);
    }

    pub fn settings(&self) -> Settings {
        self.state.borrow().settings.clone()
    }

    /// Starts background work once the event loop runs.
    pub fn start(self: &Rc<Self>) {
        let app = Rc::clone(self);
        self.tick
            .start(TimerMode::Repeated, TICK, move || app.on_tick());

        let mut engine = self.engine.clone();
        self.spawn(
            async move {
                let _ = engine.wait_for(Option::is_some).await;
                let state = engine.borrow().clone();
                state.unwrap_or(Ok(()))
            },
            |app, result| {
                let status = match result {
                    Ok(()) => "Streams are resolved with yt-dlp.".to_owned(),
                    Err(e) => {
                        app.toast(format!("Playback engine unavailable: {e}"));
                        format!("Playback engine unavailable: {e}")
                    }
                };
                app.ui().set_engine_status(status.into());
            },
        );

        let api = self.api.clone();
        self.spawn(async move { api.is_signed_in().await }, |app, signed_in| {
            app.state.borrow_mut().signed_in = signed_in;
            app.ui().set_signed_in(signed_in);
            app.navigate(Route::Home, false);
        });
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
        on!(on_sign_out, |app| app.sign_out());
        on!(on_set_prefer_aac, |app, value| {
            app.update_settings(|s| s.prefer_aac = value);
            app.state.borrow_mut().playback.prefetched.clear();
        });
        on!(on_set_normalise, |app, value| {
            app.update_settings(|s| s.normalise = value);
            app.player.set_normalise(value);
        });
        on!(on_set_discord, |app, value| {
            app.update_settings(|s| s.discord = value);
            app.update_presence();
        });
        on!(on_set_autoplay, |app, value| app
            .update_settings(|s| s.autoplay = value));
        on!(on_player_event, |app, kind, message| app
            .on_player_event(&kind, message.to_string()));
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
        if let Route::Search(query) = &route {
            ui.set_search_text(query.into());
        }

        let signed_in = self.state.borrow().signed_in;
        if route == Route::Library && !signed_in {
            ui.set_page_loading(false);
            ui.set_page_error("Sign in to see your playlists, albums and artists.".into());
            ui.set_sign_in_open(true);
            return;
        }

        let api = self.api.clone();
        self.spawn(
            async move { api.page(&route, signed_in).await },
            move |app, result| {
                if app.state.borrow().page_generation != generation {
                    return;
                }
                let ui = app.ui();
                ui.set_page_loading(false);
                match result {
                    Ok(page) => app.show_page(page),
                    Err(e) => {
                        log::warn!("page failed: {e}");
                        ui.set_page_error(e.into());
                    }
                }
            },
        );
    }

    fn reload(self: &Rc<Self>) {
        let route = self.state.borrow().route.clone();
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
        let generation = self.state.borrow().page_generation;
        let mut rows = Vec::new();
        // (row, thumbnail) for track artwork to fetch.
        let mut track_art = Vec::new();
        // Each card model with the thumbnails its cards still need.
        let mut card_art: Vec<CardArt> = Vec::new();

        if let Some(header) = &page.header {
            rows.push(RowItem {
                kind: RowKind::Header,
                title: s(&header.title),
                subtitle: s(&header.subtitle),
                meta: s(&header.meta),
                description: s(&header.description),
                art: header
                    .thumb
                    .as_deref()
                    .and_then(|url| images::cached(url, 544))
                    .unwrap_or_default(),
                round: header.round,
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
                    for (i, track) in tracks.iter().enumerate() {
                        let number = track.track_nr.map_or(i + 1, usize::from);
                        if track.thumb.is_some() && !*numbered {
                            track_art.push((rows.len(), track.thumb.clone()));
                        }
                        rows.push(RowItem {
                            kind: RowKind::Track,
                            track: track_item(track, number),
                            numbered: *numbered,
                            section: section_index as i32,
                            index: i as i32,
                            ..Default::default()
                        });
                    }
                }
                Section::Cards { cards, layout, .. } => {
                    let model = Rc::new(VecModel::from(
                        cards.iter().map(card_item).collect::<Vec<_>>(),
                    ));
                    card_art.push((
                        Rc::clone(&model),
                        cards.iter().map(|c| c.thumb.clone()).collect(),
                    ));
                    rows.push(RowItem {
                        kind: match layout {
                            CardLayout::Carousel => RowKind::Carousel,
                            CardLayout::Grid => RowKind::Grid,
                            CardLayout::Chips => RowKind::Chips,
                        },
                        cards: ModelRc::from(model),
                        section: section_index as i32,
                        ..Default::default()
                    });
                }
            }
        }

        let header_thumb = page.header.as_ref().and_then(|h| h.thumb.clone());
        self.state.borrow_mut().page = page;
        self.rows.set_vec(rows);
        let scroll = self.state.borrow_mut().restore_scroll.take().unwrap_or(0.0);
        self.ui().set_page_scroll(scroll);

        let still_current = move |app: &Rc<Self>| app.state.borrow().page_generation == generation;
        self.load_image(header_thumb, 544, move |app, image| {
            if still_current(app) && app.rows.row_count() > 0 {
                let mut row = app.rows.row_data(0).unwrap_or_default();
                if row.kind == RowKind::Header {
                    row.art = image;
                    app.rows.set_row_data(0, row);
                }
            }
        });
        for (row_index, url) in track_art {
            self.load_image(url, api::ROW_THUMB, move |app, image| {
                if !still_current(app) {
                    return;
                }
                if let Some(mut row) = app.rows.row_data(row_index) {
                    row.track.art = image;
                    app.rows.set_row_data(row_index, row);
                }
            });
        }
        for (model, urls) in card_art {
            for (i, url) in urls.into_iter().enumerate() {
                let model = Rc::clone(&model);
                self.load_image(url, api::CARD_THUMB, move |_, image| {
                    if let Some(mut card) = model.row_data(i) {
                        card.art = image;
                        model.set_row_data(i, card);
                    }
                });
            }
        }
    }

    fn suggest(self: &Rc<Self>, query: String) {
        let generation = {
            let mut state = self.state.borrow_mut();
            state.suggest_generation += 1;
            state.suggest_generation
        };
        if query.trim().chars().count() < 2 {
            self.ui().set_suggestions(ModelRc::default());
            return;
        }
        let api = self.api.clone();
        self.spawn(
            async move {
                tokio::time::sleep(SUGGEST_DELAY).await;
                api.suggestions(&query).await
            },
            move |app, terms| {
                if app.state.borrow().suggest_generation != generation {
                    return;
                }
                let terms: Vec<SharedString> = terms.iter().take(8).map(s).collect();
                app.ui()
                    .set_suggestions(ModelRc::from(Rc::new(VecModel::from(terms))));
            },
        );
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
        self.state.borrow_mut().queue.set(vec![track], 0, false);
        self.start_current();
        let api = self.api.clone();
        let seed = id.clone();
        self.spawn(
            async move { api.track_radio(&seed).await },
            move |app, result| match result {
                Ok(tracks) => {
                    let still_first = app.state.borrow().queue.get(0).is_some_and(|t| t.id == id);
                    if still_first {
                        app.append_unique(tracks);
                    }
                }
                Err(e) => log::warn!("radio failed: {e}"),
            },
        );
    }

    fn append_unique(self: &Rc<Self>, tracks: Vec<Track>) {
        {
            let mut state = self.state.borrow_mut();
            let known: HashSet<String> = state.queue.items().map(|t| t.id.clone()).collect();
            let shuffle = state.settings.shuffle;
            let fresh: Vec<Track> = tracks
                .into_iter()
                .filter(|t| !known.contains(&t.id))
                .collect();
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

    fn play_card(self: &Rc<Self>, kind: &str, id: String, radio: bool) {
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
            playback.generation
        };
        self.player.stop();
        self.player.set_paused(false);

        let ui = self.ui();
        ui.set_has_track(true);
        ui.set_np_title(s(&track.title));
        ui.set_np_artists(s(track.artist_names()));
        ui.set_np_album(s(track.album.as_ref().map_or("", |(_, n)| n.as_str())));
        ui.set_np_has_artist(track.first_artist_id().is_some());
        ui.set_np_has_album(track.album.is_some());
        ui.set_np_duration(track.duration.unwrap_or(0) as f32);
        ui.set_np_position(0.0);
        ui.set_np_buffered(0.0);
        ui.set_np_playing(true);
        ui.set_np_loading(true);
        ui.set_np_quality(SharedString::new());
        ui.set_np_art(
            track
                .thumb
                .as_deref()
                .and_then(|u| images::cached(u, api::ROW_THUMB))
                .unwrap_or_default(),
        );
        ui.global::<ApiGlobal>().set_playing_id(s(&track.id));
        let id = track.id.clone();
        self.load_image(track.large_thumb(), 240, move |app, image| {
            if app
                .state
                .borrow()
                .queue
                .current()
                .is_some_and(|t| t.id == id)
            {
                app.ui().set_np_art(image);
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
        self.update_presence();
        self.state.borrow_mut().lyrics_for = None;
        if ui.get_panel_open() && ui.get_panel_tab() == 1 {
            self.load_lyrics();
        }

        let prefetched = {
            let mut state = self.state.borrow_mut();
            state
                .playback
                .prefetched
                .remove(&track.id)
                .filter(|(_, at)| at.elapsed() < PREFETCH_FRESH)
                .map(|(stream, _)| stream)
        };
        let resolve = self.resolve_future(track.id.clone(), prefetched);
        let api = self.api.clone();
        let id = track.id.clone();
        self.spawn(
            async move {
                let (stream, loudness) = tokio::join!(resolve, api.loudness_db(&id));
                stream.map(|s| (s, loudness))
            },
            move |app, result| {
                if app.state.borrow().playback.generation != generation {
                    return;
                }
                match result {
                    Ok((stream, loudness_db)) => app.load_stream(stream, loudness_db),
                    Err(e) => app.playback_failed(&track.title, &e),
                }
            },
        );
    }

    /// Resolves a stream once yt-dlp is ready, reusing a prefetched one.
    fn resolve_future(
        &self,
        video_id: String,
        prefetched: Option<ResolvedStream>,
    ) -> impl Future<Output = Result<ResolvedStream, String>> + Send + 'static {
        let mut engine = self.engine.clone();
        let (prefer_aac, signed_in) = {
            let state = self.state.borrow();
            (state.settings.prefer_aac, state.signed_in)
        };
        async move {
            if let Some(stream) = prefetched {
                return Ok(stream);
            }
            let ready = engine
                .wait_for(Option::is_some)
                .await
                .map_err(|e| e.to_string())?
                .clone();
            ready.unwrap_or(Ok(()))?;
            tokio::task::spawn_blocking(move || {
                let cookies = settings::cookies_path();
                let cookies = (signed_in && cookies.exists()).then_some(cookies.as_path());
                streams::resolve(&video_id, cookies, prefer_aac)
            })
            .await
            .map_err(|e| e.to_string())?
        }
    }

    fn load_stream(self: &Rc<Self>, stream: ResolvedStream, loudness_db: Option<f32>) {
        let codec = match stream.codec.split('.').next().unwrap_or_default() {
            "mp4a" => "AAC".to_owned(),
            other => other.to_uppercase(),
        };
        let quality = match stream.bitrate_kbps {
            Some(kbps) if !codec.is_empty() => format!("{codec} {}", kbps.round()),
            _ => codec,
        };
        if let Some(duration) = stream.duration {
            let mut state = self.state.borrow_mut();
            if state.playback.duration <= 0.0 {
                state.playback.duration = duration;
                drop(state);
                self.ui().set_np_duration(duration as f32);
            }
        }
        self.ui().set_np_quality(quality.into());
        self.player.load(StreamSpec {
            url: stream.url,
            size: stream.size,
            mime: stream.mime,
            headers: stream.headers,
            loudness_db,
        });
    }

    fn playback_failed(self: &Rc<Self>, title: &str, error: &str) {
        log::warn!("playback failed: {error}");
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
            self.ui().set_np_playing(false);
        }
    }

    fn on_player_event(self: &Rc<Self>, kind: &str, message: String) {
        match kind {
            "started" => {
                {
                    let mut state = self.state.borrow_mut();
                    state.playback.started = true;
                    state.playback.failures = 0;
                }
                self.ui().set_np_loading(false);
                self.update_media_state();
                self.update_presence();
                self.extend_with_radio_if_ending();
                self.prefetch_next();
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
                self.playback_failed(&title, &message);
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
        ui.set_np_playing(false);
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
        self.ui().set_np_playing(!paused);
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
        if !ui.get_has_track() {
            return;
        }
        let started = self.state.borrow().playback.started;
        if started {
            ui.set_np_position(self.player.position() as f32);
            ui.set_np_buffered(self.player.buffered());
            let stalled = self.player.is_buffering() && !self.player.is_paused();
            ui.set_np_loading(stalled && self.player.buffered() < 1.0);
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
        let (items, thumbs, generation) = {
            let mut state = self.state.borrow_mut();
            state.queue_generation += 1;
            let current = state.queue.current_index();
            let items: Vec<QueueItem> = state
                .queue
                .items()
                .enumerate()
                .map(|(i, t)| QueueItem {
                    title: s(&t.title),
                    artists: s(t.artist_names()),
                    duration: s(t.duration.map(api::format_duration).unwrap_or_default()),
                    art: t
                        .thumb
                        .as_deref()
                        .and_then(|u| images::cached(u, api::ROW_THUMB))
                        .unwrap_or_default(),
                    current: Some(i) == current,
                })
                .collect();
            let thumbs: Vec<Option<String>> =
                state.queue.items().map(|t| t.thumb.clone()).collect();
            (items, thumbs, state.queue_generation)
        };
        self.ui().set_queue_current(
            self.state
                .borrow()
                .queue
                .current_index()
                .map_or(-1, |i| i as i32),
        );
        self.queue_model.set_vec(items);
        for (i, url) in thumbs.into_iter().enumerate() {
            if url
                .as_deref()
                .is_some_and(|u| images::cached(u, api::ROW_THUMB).is_some())
            {
                continue;
            }
            self.load_image(url, api::ROW_THUMB, move |app, image| {
                if app.state.borrow().queue_generation != generation {
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
        let pasted = cookie.clone();
        self.spawn(
            async move { api.sign_in(&pasted).await },
            move |app, result| {
                let ui = app.ui();
                ui.set_signing_in(false);
                match result {
                    Ok(()) => {
                        let file = settings::netscape_cookies(&cookie);
                        if let Err(e) = std::fs::write(settings::cookies_path(), file) {
                            log::warn!("could not save cookies for yt-dlp: {e}");
                        }
                        app.state.borrow_mut().signed_in = true;
                        app.state.borrow_mut().playback.prefetched.clear();
                        ui.set_signed_in(true);
                        ui.set_sign_in_open(false);
                        app.toast("Signed in");
                        app.navigate(Route::Library, true);
                    }
                    Err(e) => ui.set_sign_in_error(format!("Sign-in failed: {e}").into()),
                }
            },
        );
    }

    fn sign_out(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(async move { api.sign_out().await }, |app, ()| {
            let _ = std::fs::remove_file(settings::cookies_path());
            app.state.borrow_mut().signed_in = false;
            app.state.borrow_mut().playback.prefetched.clear();
            let ui = app.ui();
            ui.set_signed_in(false);
            ui.set_settings_open(false);
            app.toast("Signed out");
            app.navigate(Route::Home, true);
        });
    }
}
