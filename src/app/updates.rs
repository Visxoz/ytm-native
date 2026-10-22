//! Checking GitHub for a newer release and installing it.

use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use slint::ComponentHandle;

use super::{App, s};
use crate::update;

pub(super) fn up_to_date_status(checking: bool) -> String {
    if checking {
        format!(
            "ytm-native {}. Checks GitHub for new versions.",
            update::CURRENT
        )
    } else {
        format!("ytm-native {}. Automatic checks are off.", update::CURRENT)
    }
}

impl App {
    /// Looks for a newer release, when checks are on and none is waiting.
    pub(super) fn check_for_update(self: &Rc<Self>) {
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
    pub(super) fn install_update(self: &Rc<Self>) {
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
}
