use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::{ModelRc, SharedString, TimerMode, VecModel};

use super::{App, HistoryEntry, s};
use crate::{
    AppWindow,
    api::{self, Page, Route},
    settings,
};

const PAGE_FRESH_FOR: Duration = Duration::from_secs(5 * 60);
const PAGE_CACHE_SIZE: usize = 12;
const SUGGEST_DELAY: Duration = Duration::from_millis(150);

impl App {
    pub(super) fn navigate(self: &Rc<Self>, route: Route, push: bool) {
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
        ui.invoke_close_search();
        let signed_in = self.state.borrow().signed_in;
        ui.set_page_shape(
            match route {
                Route::Album(_) => "album",
                // Song lists with a header, as a playlist's.
                Route::Playlist(_) | Route::History | Route::LocalFiles | Route::Downloads => {
                    "playlist"
                }
                Route::Artist(_) => "artist",
                Route::Genre(_) => "genre",
                Route::Home if signed_in => "home",
                Route::Home => "trending",
                Route::Explore => "chips",
                Route::Library => "grid",
                Route::Search(_) => "search",
            }
            .into(),
        );
        ui.set_page_loading(true);
        ui.set_page_error(SharedString::new());
        match &route {
            Route::Search(query) => ui.set_search_text(query.into()),
            _ => ui.set_search_text(SharedString::new()),
        }

        if route == Route::Library && !signed_in {
            ui.set_page_loading(false);
            ui.set_page_error("Sign in to see your playlists, albums and artists.".into());
            ui.set_sign_in_open(true);
            return;
        }
        // Signed in, the History page is the account's own, fetched fresh
        // each visit; the local one stands in when plays aren't sent there.
        let online_history = signed_in && self.state.borrow().settings.youtube_history;
        match route {
            Route::History if !online_history => {
                self.show_or_empty(self.history_page(), "Songs you play will show up here.");
                return;
            }
            Route::Downloads => {
                self.show_downloads();
                return;
            }
            Route::LocalFiles => {
                self.load_local_files(generation);
                return;
            }
            _ => {}
        }

        // An expired copy shows while a fresh one loads behind it.
        if let Some((page, fresh, cached_at)) = self.cached_page(&route) {
            ui.set_page_loading(false);
            self.show_page(page);
            if !fresh {
                self.refresh_page(route, signed_in, generation, cached_at);
            }
            return;
        }

        if route == Route::Home && signed_in {
            self.load_personal_home(generation);
            return;
        }

        let api = self.api.clone();
        self.spawn(
            async move {
                let page = api.page(&route, signed_in).await;
                (route, page)
            },
            // The history isn't cached, so a song just played shows.
            move |app, (route, result)| {
                let cache_as = (route != Route::History).then_some(route);
                app.page_loaded(generation, result, cache_as)
            },
        );
    }

    /// The cached copy of `route`, whether it is fresh, and when it was cached.
    /// One left half-loaded is none once expired, as its continuation has too.
    pub(super) fn cached_page(&self, route: &Route) -> Option<(Page, bool, Instant)> {
        self.state
            .borrow()
            .page_cache
            .get(route)
            .and_then(|(page, at)| {
                let fresh = at.elapsed() < PAGE_FRESH_FOR;
                (fresh || page.more.is_none()).then(|| (page.clone(), fresh, *at))
            })
    }

    fn page_loaded(
        self: &Rc<Self>,
        generation: u64,
        result: api::Result<Page>,
        cache_as: Option<Route>,
    ) {
        if self.state.borrow().page_generation != generation {
            return;
        }
        let ui = self.ui();
        ui.set_page_loading(false);
        match result {
            Ok(page) => {
                if let Some(route) = cache_as {
                    self.cache_page(route, page.clone());
                }
                self.show_page(page);
            }
            Err(e) => {
                log::warn!("page failed: {e}");
                ui.set_page_error(e.into());
            }
        }
    }

    /// Fetches the whole of the expired page on screen, cached at `cached_at`,
    /// and swaps it in where the user has scrolled to should it differ. A
    /// failure keeps the copy.
    fn refresh_page(
        self: &Rc<Self>,
        route: Route,
        signed_in: bool,
        generation: u64,
        cached_at: Instant,
    ) {
        let home = route == Route::Home && signed_in;
        if home {
            // As with each fresh feed, so a changed picture shows.
            self.load_account_photo();
        }
        let api = self.api.clone();
        let asked = route.clone();
        self.spawn(
            async move {
                let mut page = api.page(&asked, signed_in).await?;
                while page.more.is_some() {
                    let whole;
                    (page, whole) = api.complete(page).await;
                    if !whole {
                        return Err("part of it failed".to_owned());
                    }
                }
                Ok(page)
            },
            move |app, result: api::Result<Page>| {
                let page = match result {
                    Ok(page) => page,
                    Err(e) => {
                        log::warn!("could not refresh the page: {e}");
                        return;
                    }
                };
                let (ours, swap) = {
                    let state = app.state.borrow();
                    // Gone, as after a like or signing out, it is to load
                    // afresh. Still ours, it is cached even once the user has
                    // moved on. Replaced while still on screen, it was by an
                    // earlier refresh from a visit left and come back to.
                    let Some((_, at)) = state.page_cache.get(&route) else {
                        return;
                    };
                    let ours = *at == cached_at;
                    let current = state.page_generation == generation;
                    if !ours && !current {
                        return;
                    }
                    // Home's shelves differ on every load and Home never moves
                    // what is shown, so its fresh feed waits for the next visit.
                    let swap = current
                        && !home
                        && (state.page.header != page.header
                            || state.page.sections != page.sections);
                    (ours, swap)
                };
                if ours && home {
                    app.save_home(page.sections.clone());
                }
                if !swap {
                    if ours {
                        app.cache_page(route, page);
                    }
                    return;
                }
                if ours {
                    app.cache_page(route, page.clone());
                }
                {
                    let mut state = app.state.borrow_mut();
                    state.restore_scroll = Some(app.ui().get_page_scroll());
                    // Rows move, so art still on its way to their old places is dropped.
                    state.page_generation += 1;
                }
                app.show_page(page);
            },
        );
    }

    /// Should the feed fail before any of it shows, the last whole one stands
    /// in, or the charts: rather that than a fresh one missing shelves.
    fn load_personal_home(self: &Rc<Self>, generation: u64) {
        // Asked with each fresh feed, so a changed or failed picture shows
        // within minutes.
        self.load_account_photo();
        let api = self.api.clone();
        let ahead = self.home_ahead.take();
        self.spawn(
            async move {
                let result = match ahead {
                    Some(ahead) => ahead.await.unwrap_or_else(|e| Err(e.to_string())),
                    None => api.page(&Route::Home, true).await,
                };
                result.map_err(|e| {
                    log::warn!("could not load the personal home feed: {e}");
                    settings::load_home().map(api::arrange_home)
                })
            },
            move |app, result| match result {
                Ok(page) => {
                    // A feed still arriving is saved once whole.
                    if page.more.is_none() && app.state.borrow().page_generation == generation {
                        app.save_home(page.sections.clone());
                    }
                    app.page_loaded(generation, Ok(page), Some(Route::Home));
                }
                Err(Some(sections)) => {
                    let page = Page {
                        header: None,
                        sections,
                        more: None,
                    };
                    app.page_loaded(generation, Ok(page), None);
                }
                Err(None) => {
                    if app.state.borrow().page_generation == generation {
                        app.show_public_home(generation);
                    }
                }
            },
        );
    }

    /// Not cached, so Home asks for the personal feed again next time.
    fn show_public_home(self: &Rc<Self>, generation: u64) {
        let api = self.api.clone();
        self.spawn(
            async move { api.page(&Route::Home, false).await },
            move |app, result| app.page_loaded(generation, result, None),
        );
    }

    pub(super) fn save_home(self: &Rc<Self>, sections: Vec<api::Section>) {
        if !self.state.borrow().signed_in {
            return;
        }
        self.spawn(async move { settings::save_home(&sections) }, |_, ()| {});
    }

    pub(super) fn cache_page(&self, route: Route, page: Page) {
        let mut state = self.state.borrow_mut();
        let cache = &mut state.page_cache;
        // Expired pages are kept to show while they reload, but not those
        // left half-loaded, which `navigate` loads afresh.
        cache.retain(|_, (page, at)| at.elapsed() < PAGE_FRESH_FOR || page.more.is_none());
        if !cache.contains_key(&route)
            && cache.len() >= PAGE_CACHE_SIZE
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(r, _)| r.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(route, (page, Instant::now()));
    }

    /// A page built on this PC, or the message in its place.
    pub(super) fn show_or_empty(self: &Rc<Self>, page: Option<Page>, empty: &str) {
        let ui = self.ui();
        ui.set_page_loading(false);
        match page {
            Some(page) => {
                // Shown again with songs, the page may have been empty.
                ui.set_page_error(SharedString::new());
                self.show_page(page)
            }
            None => {
                // No page takes it, so it would scroll the next one.
                self.state.borrow_mut().restore_scroll = None;
                ui.set_page_error(empty.into());
            }
        }
    }

    pub(super) fn reload(self: &Rc<Self>) {
        let route = self.state.borrow().route.clone();
        self.state.borrow_mut().page_cache.remove(&route);
        self.navigate(route, false);
    }

    /// For signing in or out; navigating to the page already shown would do nothing.
    pub(super) fn show_fresh(self: &Rc<Self>, route: Route) {
        if self.state.borrow().route == route {
            self.reload();
        } else {
            self.navigate(route, true);
        }
    }

    pub(super) fn go_back(self: &Rc<Self>) {
        self.step_history(false);
    }

    pub(super) fn go_forward(self: &Rc<Self>) {
        self.step_history(true);
    }

    fn step_history(self: &Rc<Self>, forward: bool) {
        let scroll = self.ui().get_page_scroll();
        let entry = {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            let (from, to) = if forward {
                (&mut state.forward, &mut state.back)
            } else {
                (&mut state.back, &mut state.forward)
            };
            let Some(entry) = from.pop() else {
                return;
            };
            to.push(HistoryEntry {
                route: state.route.clone(),
                scroll,
            });
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

    pub(super) fn search(self: &Rc<Self>, query: &str) {
        self.drop_suggestions();
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        let recorded = self.state.borrow_mut().searches.record(query);
        self.navigate(Route::Search(query.to_owned()), true);
        if recorded {
            self.recent_searches_changed();
        }
    }

    pub(super) fn forget_search(&self, query: &str) {
        let removed = self.state.borrow_mut().searches.remove(query);
        if removed {
            self.recent_searches_changed();
        }
    }

    /// Saves the recent searches at once rather than through
    /// `persist_soon`, which would rewrite the whole queue too.
    fn recent_searches_changed(&self) {
        self.state.borrow_mut().searches.save();
        self.show_recent_searches();
    }

    pub(super) fn show_recent_searches(&self) {
        let recent: VecModel<SharedString> =
            self.state.borrow().searches.queries().map(s).collect();
        self.ui()
            .set_recent_searches(ModelRc::from(Rc::new(recent)));
    }

    fn drop_suggestions(&self) {
        self.suggest_timer.stop();
        self.state.borrow_mut().suggest_generation += 1;
        self.ui().set_suggestions(ModelRc::default());
    }

    pub(super) fn suggest(self: &Rc<Self>, query: String) {
        if query.trim().chars().count() < 2 {
            self.drop_suggestions();
            return;
        }
        let generation = {
            let mut state = self.state.borrow_mut();
            state.suggest_generation += 1;
            state.suggest_generation
        };
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
}
