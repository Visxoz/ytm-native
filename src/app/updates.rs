use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use super::{App, s};
use crate::update;

pub(super) fn up_to_date_status(checking: bool) -> String {
    if checking {
        format!("Lily {}. Checks GitHub for new versions.", update::CURRENT)
    } else {
        format!("Lily {}. Automatic checks are off.", update::CURRENT)
    }
}

impl App {
    pub(super) fn check_for_update(self: &Rc<Self>) {
        if !self.state.borrow().settings.check_updates || self.update.borrow().is_some() {
            return;
        }
        self.spawn(update::check(), |app, result| match result {
            Ok(Some(found)) => {
                log::info!("Lily {} is available", found.version);
                let ui = app.ui();
                ui.set_update_version(s(&found.version));
                ui.set_update_status(
                    format!(
                        "Lily {} is available. Installing restarts the app.",
                        found.version
                    )
                    .into(),
                );
                ui.set_update_prompt(true);
                *app.update.borrow_mut() = Some(found);
            }
            Ok(None) => {}
            Err(e) => log::debug!("{e}"),
        });
    }

    /// Closes the app as its close button does; the update takes over on exit.
    pub(super) fn install_update(self: &Rc<Self>) {
        let Some(found) = self.update.borrow().clone() else {
            return;
        };
        let ui = self.ui();
        ui.set_update_busy(true);
        ui.set_update_progress(0.0);
        ui.set_update_status(format!("Downloading version {}…", found.version).into());
        let weak = self.ui.clone();
        let shown = Arc::new(AtomicU8::new(0));
        let progress = move |fraction: f32| {
            let percent = (fraction * 100.0) as u8;
            if shown.swap(percent, Ordering::Relaxed) != percent {
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_update_progress(f32::from(percent) / 100.0);
                    ui.set_update_status(format!("Downloading… {percent}%").into());
                });
            }
        };
        // Only the result comes back, so a panic ends the wait as a failure.
        let asked = found.clone();
        let download = async move { update::download(&asked, progress).await };
        self.spawn(download, move |app, result| {
            let ui = app.ui();
            match result.and_then(|path| update::stage(&found, path)) {
                Ok(()) => {
                    ui.set_update_status("Restarting to finish the update…".into());
                    app.quit();
                }
                Err(e) => {
                    ui.set_update_busy(false);
                    ui.set_update_status(format!("The update failed: {e}").into());
                }
            }
        });
    }
}
