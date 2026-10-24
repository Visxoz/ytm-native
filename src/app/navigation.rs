//! Moving between pages: history, the page cache and search suggestions.

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

/// How long a loaded page is reused when navigating back to it.
const PAGE_CACHE_FOR: Duration = Duration::from_secs(5 * 60);
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
        if route == Route::History {
            ui.set_page_loading(false);
            match self.history_page() {
                Some(page) => self.show_page(page),
                None => ui.set_page_error("Songs you play will show up here.".into()),
            }
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
            move |app, (route, result)| app.page_loaded(generation, result, Some(route)),
        );
    }

    /// Shows a page that has loaded, or why it did not, unless the user
    /// has moved on meanwhile. With `cache_as`, it is kept for going back.
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

    /// The signed-in feed in one request. Should YouTube refuse it, the feed
    /// as it last loaded shows at once while the retries go on behind it,
    /// and what they fetch waits for the next visit, so the page never
    /// changes under the reader.
    fn load_personal_home(self: &Rc<Self>, generation: u64) {
        let api = self.api.clone();
        self.spawn(
            async move { api.home_first_try().await },
            move |app, result| {
                if app.state.borrow().page_generation != generation {
                    return;
                }
                match result {
                    Ok(page) => app.show_personal_home(page),
                    Err(e) => {
                        log::debug!("the personal home feed failed at first: {e}");
                        match settings::load_home() {
                            Some(sections) => {
                                app.ui().set_page_loading(false);
                                app.show_page(Page {
                                    header: None,
                                    sections,
                                    more: None,
                                });
                                app.refresh_personal_home();
                            }
                            None => app.retry_personal_home(generation),
                        }
                    }
                }
            },
        );
    }

    fn show_personal_home(self: &Rc<Self>, page: Page) {
        if page.more.is_none() {
            self.save_home(&page);
        }
        self.ui().set_page_loading(false);
        self.cache_page(Route::Home, page.clone());
        self.show_page(page);
    }

    /// With nothing saved to show, waits out the retries, then the charts.
    fn retry_personal_home(self: &Rc<Self>, generation: u64) {
        let api = self.api.clone();
        self.spawn(
            async move { api.page(&Route::Home, true).await },
            move |app, result| {
                if app.state.borrow().page_generation != generation {
                    return;
                }
                match result {
                    Ok(page) => app.show_personal_home(page),
                    Err(e) => {
                        log::warn!("could not load the personal home feed: {e}");
                        app.show_public_home(generation);
                    }
                }
            },
        );
    }

    /// Fetches the feed behind the saved one, for the next visit. Kept
    /// whole by the next `show_page`, which also completes and saves it.
    fn refresh_personal_home(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(
            async move { api.page(&Route::Home, true).await },
            |app, result| match result {
                Ok(page) if app.state.borrow().signed_in => {
                    if page.more.is_none() {
                        app.save_home(&page);
                    }
                    app.cache_page(Route::Home, page);
                }
                Ok(_) => {}
                Err(e) => log::warn!("kept the saved home feed: {e}"),
            },
        );
    }

    /// The charts, in place of a personal feed that failed with none saved.
    /// Not cached, so Home asks for the feed again next time.
    fn show_public_home(self: &Rc<Self>, generation: u64) {
        let api = self.api.clone();
        self.spawn(
            async move { api.page(&Route::Home, false).await },
            move |app, result| app.page_loaded(generation, result, None),
        );
    }

    /// Saves the signed-in feed once it has loaded in full, off the UI
    /// thread.
    pub(super) fn save_home(self: &Rc<Self>, page: &Page) {
        if !self.state.borrow().signed_in {
            return;
        }
        let sections = page.sections.clone();
        self.spawn(async move { settings::save_home(&sections) }, |_, ()| {});
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

    pub(super) fn reload(self: &Rc<Self>) {
        let route = self.state.borrow().route.clone();
        self.state.borrow_mut().page_cache.remove(&route);
        self.navigate(route, false);
    }

    /// Shows `route` loaded afresh, as signing in or out changes what it
    /// holds. Navigating to the page already shown would do nothing.
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

    /// Returns to the page one step back or forward, where it was scrolled,
    /// leaving the current one on the other side.
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

    pub(super) fn suggest(self: &Rc<Self>, query: String) {
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
}
