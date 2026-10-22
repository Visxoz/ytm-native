//! Moving between pages: history, the page cache and search suggestions.

use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use slint::{ModelRc, SharedString, TimerMode, VecModel};

use super::{App, HistoryEntry, s};
use crate::{
    AppWindow,
    api::{Page, Route},
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

    pub(super) fn go_forward(self: &Rc<Self>) {
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
