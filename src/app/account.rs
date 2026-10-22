//! Signing in and out, with pasted cookies or Google's sign-in window, and
//! the account's library and picture.

use std::{cell::Cell, rc::Rc, time::Duration};

use slint::{ComponentHandle, Image, Model, SharedString, Timer, TimerMode};

use super::{App, items::card_item};
use crate::{
    CardItem, api::Route, google_sign_in::GoogleSignIn, images, settings, streams::premium,
};

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
                        app.premium.set_cookie(Some(header));
                        app.update_settings(|s| s.premium_account = false);
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

    /// Fills the sidebar with the signed-in user's playlists.
    pub(super) fn load_library(self: &Rc<Self>) {
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
    pub(super) fn sign_in_expired(self: &Rc<Self>) {
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

    /// Puts the account's profile picture on the account button.
    pub(super) fn load_account_photo(self: &Rc<Self>) {
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

    pub(super) fn sign_out(self: &Rc<Self>) {
        let api = self.api.clone();
        self.spawn(async move { api.sign_out().await }, |app, ()| {
            app.state.borrow_mut().signed_in = false;
            app.state.borrow_mut().page_cache.clear();
            app.library.set_vec(Vec::new());
            settings::remove_cookie();
            app.premium.set_cookie(None);
            app.update_settings(|s| s.premium_account = false);
            let ui = app.ui();
            ui.set_account_photo(Image::default());
            ui.set_signed_in(false);
            ui.set_settings_open(false);
            app.toast("Signed out");
            app.show_fresh(Route::Home);
        });
    }
}
