use std::{cell::Cell, rc::Rc, time::Duration};

use slint::{ComponentHandle, Image, Model, SharedString, Timer, TimerMode};

use super::{App, items::card_item};
use crate::{
    Api as ApiGlobal, CardItem,
    api::{Card, Route},
    google_sign_in::GoogleSignIn,
    images, settings,
};

/// The least time between cookie checks, after one that didn't find the
/// cookie rejected.
const SIGN_IN_RECHECK_AFTER: Duration = Duration::from_secs(10 * 60);

impl App {
    pub(super) fn sign_in(self: &Rc<Self>, cookie: String) {
        if cookie.trim().is_empty() {
            self.ui()
                .set_sign_in_error("Paste your cookies first.".into());
            return;
        }
        let ui = self.ui();
        ui.set_signing_in(true);
        ui.set_sign_in_error(SharedString::new());
        let api = self.api.clone();
        self.spawn(
            async move { api.sign_in(&cookie).await },
            move |app, result| {
                let ui = app.ui();
                ui.set_signing_in(false);
                match result {
                    Ok(()) => {
                        {
                            let mut state = app.state.borrow_mut();
                            state.signed_in = true;
                            state.account += 1;
                            state.page_cache.clear();
                        }
                        // It may be another account's.
                        settings::remove_account_data();
                        app.load_library();
                        app.load_account_photo();
                        app.premium.reset();
                        app.update_settings(|s| {
                            s.premium_account = false;
                            s.no_premium = false;
                            s.account_photo = None;
                        });
                        app.warm_premium();
                        ui.global::<ApiGlobal>().set_signed_in(true);
                        ui.set_sign_in_open(false);
                        if let Some(id) = app.playing_id() {
                            app.load_rating(id);
                        }
                        app.toast("Signed in");
                        app.show_fresh(Route::Library);
                    }
                    Err(e) => ui.set_sign_in_error(format!("Sign-in failed: {e}").into()),
                }
            },
        );
    }

    pub(super) fn sign_in_with_google(self: &Rc<Self>) {
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
        let attempts = Cell::new(0);
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

    pub(super) fn load_library(self: &Rc<Self>) {
        let api = self.api.clone();
        // Signing in or out meanwhile makes the playlists another account's.
        let account = self.state.borrow().account;
        self.spawn(
            async move { api.library_playlists().await },
            move |app, result| match result {
                Ok(cards) if app.state.borrow().account == account => {
                    app.show_library(&cards);
                    app.spawn(async move { settings::save_library(&cards) }, |_, ()| {});
                }
                Ok(_) => {}
                Err(e) => log::warn!("{e}"),
            },
        );
    }

    /// The sidebar's playlists, as saved by the last run or freshly loaded.
    pub(super) fn show_library(self: &Rc<Self>, cards: &[Card]) {
        let items: Vec<CardItem> = cards.iter().map(|c| card_item(c, images::ROW)).collect();
        self.library.set_vec(items);
        for (i, card) in cards.iter().enumerate() {
            // The saved list is replaced by the fresh one while its art loads.
            let id = SharedString::from(card.id.as_str());
            self.load_image(card.thumb.clone(), images::ROW, move |app, image| {
                if let Some(mut item) = app.library.row_data(i)
                    && item.id == id
                {
                    item.art = image;
                    app.library.set_row_data(i, item);
                }
            });
        }
    }

    /// Waits for YouTube to answer a signed-in request as signed out, then
    /// checks the cookie, as the session can end at any time.
    pub(super) fn watch_sign_in(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(async move { api.sign_in_lost().await }, |app, ()| {
            app.check_sign_in();
        });
    }

    fn check_sign_in(self: &Rc<Self>) {
        let (signed_in, account) = {
            let state = self.state.borrow();
            (state.signed_in, state.account)
        };
        // An answer to a request sent before the cookie was removed.
        if !signed_in {
            return self.watch_sign_in();
        }
        let api = self.api.clone();
        self.spawn(
            async move {
                // A stale cookie may only need renewing, which the first
                // check of a run can beat.
                match api.cookie_valid().await {
                    Some(false) => {
                        if let Err(e) = api.rotate_cookies().await {
                            log::warn!("could not renew the sign-in: {e}");
                        }
                        api.cookie_valid().await
                    }
                    valid => valid,
                }
            },
            move |app, valid| {
                // Signing in or out meanwhile makes the answer another account's.
                if app.state.borrow().account != account {
                    return app.watch_sign_in();
                }
                if valid == Some(false) {
                    return app.sign_in_expired();
                }
                // Otherwise the cookie isn't checked again at every answer.
                let weak = Rc::downgrade(app);
                Timer::single_shot(SIGN_IN_RECHECK_AFTER, move || {
                    if let Some(app) = weak.upgrade() {
                        app.watch_sign_in();
                    }
                });
            },
        );
    }

    /// Removes the dead cookie too, so the next start doesn't sign in with it.
    fn sign_in_expired(self: &Rc<Self>) {
        log::warn!("the stored sign-in was rejected");
        let api = self.api.clone();
        let account = self.state.borrow().account;
        self.spawn(async move { api.sign_out().await }, move |app, ()| {
            // The user signed out themselves meanwhile.
            if app.state.borrow().account != account {
                return app.watch_sign_in();
            }
            app.show_signed_out();
            app.toast("YouTube signed you out");
            let reload = matches!(
                app.state.borrow().route,
                Route::Home | Route::Library | Route::History
            );
            if reload {
                app.reload();
            }
            app.watch_sign_in();
        });
    }

    fn show_signed_out(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.signed_in = false;
            state.account += 1;
            state.page_cache.clear();
        }
        settings::remove_account_data();
        self.library.set_vec(Vec::new());
        let ui = self.ui();
        ui.global::<ApiGlobal>().set_signed_in(false);
        ui.set_account_photo(Image::default());
        self.premium.reset();
        self.update_settings(|s| {
            s.premium_account = false;
            s.no_premium = false;
            s.account_photo = None;
        });
    }

    pub(super) fn load_account_photo(self: &Rc<Self>) {
        let api = self.api.clone();
        // Signing in or out meanwhile makes the photo another account's.
        let account = self.state.borrow().account;
        self.spawn(
            async move { api.account_photo().await },
            move |app, result| match result {
                Ok(Some(url)) if app.state.borrow().account == account => {
                    // Kept for the next start, written only as it changes.
                    if app.state.borrow().settings.account_photo.as_ref() != Some(&url) {
                        app.update_settings(|s| s.account_photo = Some(url.clone()));
                    }
                    app.show_account_photo(url);
                }
                Ok(_) => {}
                Err(e) => log::warn!("could not load the account's picture: {e}"),
            },
        );
    }

    pub(super) fn show_account_photo(self: &Rc<Self>, url: String) {
        // Only while it is still the account's latest picture; signing in or
        // out clears it, and the saved one may load after a fresh one.
        let shown = url.clone();
        self.load_image(Some(url), images::AVATAR, move |app, image| {
            if app.state.borrow().settings.account_photo.as_ref() == Some(&shown) {
                app.ui().set_account_photo(image);
            }
        });
    }

    /// Keeps the session from going stale or being revoked.
    pub(super) fn rotate_cookies(&self) {
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.rotate_cookies().await {
                log::warn!("could not renew the sign-in: {e}");
            }
        });
    }

    pub(super) fn sign_out(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(async move { api.sign_out().await }, |app, ()| {
            app.show_signed_out();
            app.ui().set_settings_open(false);
            app.toast("Signed out");
            app.show_fresh(Route::Home);
        });
    }
}
