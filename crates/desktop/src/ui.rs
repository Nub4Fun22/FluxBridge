//! Slint UI integration, compiled only with the `gui` feature.
//!
//! This module includes the generated Slint code (produced by `build.rs` from
//! `ui/app.slint`) and connects the `UpdateDialog` callbacks to the pure-Rust
//! updater workflow in [`crate::updater_ui`]. It is intentionally thin: all
//! real logic lives in the feature-independent modules so it stays testable on
//! a headless host.

// Pull in the Slint-generated types (MainWindow, UpdateDialog, ...).
slint::include_modules!();

use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use fluxbridge_core::ReleaseInfo;
use fluxbridge_updater::{GithubUpdater, UreqTransport};

use crate::connection::ConnectionConfig;
use crate::updater_ui::{run_update, UpdateEvent};

/// Build the main window and wire the updater dialog.
///
/// `update_events` carries messages from the startup check (and, once the user
/// accepts, from the download worker). The connection status is seeded from
/// `config`. The returned window must be run by the caller.
pub fn build_main_window(
    config: &ConnectionConfig,
    updater: GithubUpdater<UreqTransport>,
    update_events: Receiver<UpdateEvent>,
) -> Result<MainWindow, slint::PlatformError> {
    let window = MainWindow::new()?;
    window.set_connection_status(format!("Connecting to {}", config.address()).into());

    // A clonable sender so the accept worker can push progress back into the
    // same event stream the timer below drains.
    let (worker_tx, worker_rx) = mpsc::channel::<UpdateEvent>();

    // The release currently offered to the user, shared with the accept handler.
    let current_release: Rc<RefCell<Option<ReleaseInfo>>> = Rc::new(RefCell::new(None));
    // Arc so the download worker thread can share the updater with the UI.
    let updater = Arc::new(updater);

    // Dismiss ("Mai tarziu / Refuza").
    let weak = window.as_weak();
    window.on_update_dismiss(move || {
        if let Some(w) = weak.upgrade() {
            w.set_update_available(false);
        }
    });

    // Accept ("Actualizeaza acum"): download with progress, then apply.
    let weak = window.as_weak();
    let accept_release = Rc::clone(&current_release);
    let accept_updater = Arc::clone(&updater);
    let accept_tx = worker_tx.clone();
    window.on_update_accept(move || {
        let Some(w) = weak.upgrade() else { return };
        let Some(release) = accept_release.borrow().clone() else {
            return;
        };
        w.set_update_downloading(true);
        let updater = Arc::clone(&accept_updater);
        let tx = accept_tx.clone();
        // Download + apply off the UI thread so the event loop keeps running.
        std::thread::spawn(move || {
            if let Err(e) = run_update(updater.as_ref(), &release, &tx) {
                let _ = tx.send(UpdateEvent::Failed(e.to_string()));
            }
        });
    });

    // Pump both the startup-check events and the worker events into the UI.
    pump_events(&window, update_events, worker_rx, current_release);

    Ok(window)
}

/// Install a repeating timer that drains update events into the window state.
fn pump_events(
    window: &MainWindow,
    startup_events: Receiver<UpdateEvent>,
    worker_events: Receiver<UpdateEvent>,
    current_release: Rc<RefCell<Option<ReleaseInfo>>>,
) {
    let timer = Rc::new(slint::Timer::default());
    let weak = window.as_weak();
    let startup = Rc::new(startup_events);
    let worker = Rc::new(worker_events);
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(100),
        move || {
            let Some(w) = weak.upgrade() else { return };
            for event in startup.try_iter().chain(worker.try_iter()) {
                apply_event(&w, &current_release, event);
            }
        },
    );
    // Keep the timer alive for the window's lifetime.
    std::mem::forget(timer);
}

/// Apply a single [`UpdateEvent`] to the window's properties.
fn apply_event(
    w: &MainWindow,
    current_release: &Rc<RefCell<Option<ReleaseInfo>>>,
    event: UpdateEvent,
) {
    match event {
        UpdateEvent::Available(info) => {
            w.set_update_version(info.version.clone().into());
            w.set_update_notes(info.notes.clone().into());
            w.set_update_progress(0.0);
            w.set_update_available(true);
            *current_release.borrow_mut() = Some(info);
        }
        UpdateEvent::Progress(pct) => {
            w.set_update_progress(pct as f32);
        }
        UpdateEvent::Downloaded(_) => {
            w.set_update_downloading(false);
            w.set_update_available(false);
        }
        UpdateEvent::UpToDate => {}
        UpdateEvent::Failed(_) => {
            w.set_update_downloading(false);
            w.set_update_available(false);
        }
    }
}
