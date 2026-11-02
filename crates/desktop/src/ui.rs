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

/// The in-app help / "how to connect" guide shown by the Help panel.
///
/// Kept accurate to what FluxBridge actually supports: Android via ADB (USB and
/// Wi-Fi) with an mDNS auto-pairing path planned, and iOS within Apple's
/// ecosystem limits (AirPlay/mDNS mirroring, libimobiledevice/WebDriverAgent).
/// See `docs/IOS_STRATEGY.md`, `android-server/README.md` and the root README.
const HELP_TEXT: &str = "\
FluxBridge mirrors and controls your phone from this PC. Pick your platform below.

WHAT YOU NEED (all platforms)
- This FluxBridge desktop client (you are running it).
- The phone and PC reachable over USB or the same local Wi-Fi network.
- The FluxBridge device-side server running on the phone (see Android below).
- Default connection target: 127.0.0.1:27183. You can override it by passing a
  host:port argument when launching the client.

ANDROID (recommended, full control)
Requirements:
- Android phone with Developer Options and USB debugging enabled.
  Enable them in Settings > About phone > tap Build number 7 times, then
  Settings > System > Developer options > USB debugging.
- ADB (Android Debug Bridge) installed on the PC (part of the Android platform
  tools). The first USB connection prompts you on the phone to Allow / authorize
  this computer for USB debugging; accept it.
- The FluxBridge Android capture server pushed to the device (it runs via
  app_process under the shell UID, so there is no app to install).

Connect over USB:
1. Plug the phone into the PC with a USB cable and authorize the prompt.
2. Push the server and forward the port, then launch it:
     adb push fluxbridge-server.jar /data/local/tmp/
     adb forward tcp:27183 tcp:27183
     adb shell CLASSPATH=/data/local/tmp/fluxbridge-server.jar \\
         app_process / com.fluxbridge.server.Server 27183
3. Start FluxBridge; it dials 127.0.0.1:27183, which ADB tunnels to the phone.

Connect over Wi-Fi:
1. With the phone on USB once, switch ADB to TCP/IP and find the phone's IP:
     adb tcpip 5555
     adb shell ip route        (note the wlan0 address, e.g. 192.168.1.42)
2. Unplug, connect over the network, then push + forward + launch as above:
     adb connect 192.168.1.42:5555
     adb -s 192.168.1.42:5555 forward tcp:27183 tcp:27183
Note: wireless mDNS auto-pairing (no manual adb connect) is planned; until then
use the manual steps above.

iOS / iPhone (mirroring-first, Apple ecosystem limits apply)
Apple does not expose an ADB-style input-injection or free screen-capture API,
so iOS support is more limited than Android:
- Mirroring: FluxBridge acts as an AirPlay receiver discovered over mDNS /
  Bonjour. Open Control Center on the iPhone, tap Screen Mirroring, and pick
  FluxBridge; iOS streams the whole screen (full-screen only, no per-app view).
  A wired libimobiledevice path over USB is a lower-latency fallback.
- Control: synthesizing taps/swipes requires WebDriverAgent, a signed developer
  app deployed over USB via libimobiledevice. Without it, iOS is mirroring only;
  FluxBridge surfaces that state clearly.

TROUBLESHOOTING
- 'not started' / cannot connect: confirm the device-side server is running and
  that adb forward tcp:27183 is active (USB) or the phone's IP is reachable (Wi-Fi).
- Android unauthorized: re-check the Allow USB debugging prompt on the phone.
- Wrong address: pass the correct host:port when launching the client.";

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

    // Seed the in-app help panel with the connection guide. It stays hidden
    // until the user clicks the Help button.
    window.set_help_text(HELP_TEXT.into());
    window.set_help_visible(false);

    // Help button toggles the panel; the panel's Close button hides it.
    let weak = window.as_weak();
    window.on_help_toggle(move || {
        if let Some(w) = weak.upgrade() {
            w.set_help_visible(!w.get_help_visible());
        }
    });
    let weak = window.as_weak();
    window.on_help_close(move || {
        if let Some(w) = weak.upgrade() {
            w.set_help_visible(false);
        }
    });

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
            w.set_update_status("".into());
            w.set_update_status_is_error(false);
            w.set_update_available(true);
            *current_release.borrow_mut() = Some(info);
        }
        UpdateEvent::Progress(pct) => {
            w.set_update_progress(pct as f32);
        }
        UpdateEvent::Applied(_) => {
            // The installer was launched; the running binary must exit so it can
            // be replaced. Request a clean shutdown of the Slint event loop.
            w.set_update_downloading(false);
            let _ = slint::quit_event_loop();
        }
        UpdateEvent::DownloadedManual(path) => {
            // No in-app installer ran on this platform. Keep the dialog visible
            // and tell the user where the download is so they can apply it,
            // instead of hiding as if the update completed.
            w.set_update_downloading(false);
            w.set_update_status(
                format!("Downloaded to {path}. Please apply the update manually.").into(),
            );
            w.set_update_status_is_error(false);
        }
        UpdateEvent::UpToDate => {}
        UpdateEvent::Failed(msg) => {
            // Surface the failure in the dialog rather than silently hiding it.
            w.set_update_downloading(false);
            w.set_update_status(format!("Update failed: {msg}").into());
            w.set_update_status_is_error(true);
        }
    }
}
