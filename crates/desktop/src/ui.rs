//! Slint UI integration, compiled only with the `gui` feature.
//!
//! This module includes the generated Slint code (produced by `build.rs` from
//! `ui/app.slint`) and connects the `UpdateDialog` callbacks to the pure-Rust
//! updater workflow in [`crate::updater_ui`]. It is intentionally thin: all
//! real logic lives in the feature-independent modules so it stays testable on
//! a headless host.

// Pull in the Slint-generated types (MainWindow, UpdateDialog, ...).
slint::include_modules!();

/// The application icon, embedded at build time as a 256x256 RGBA PNG.
///
/// Generated from `assets/icon.svg` by the icon xtask (see README.md) and
/// committed alongside `assets/icon.ico`. Embedding the bytes means the running
/// window can set its icon without depending on a file being present next to
/// the installed executable.
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../../../assets/icon-256.png");

use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use fluxbridge_core::{DecodedFrame, ReleaseInfo};
use fluxbridge_updater::{GithubUpdater, UreqTransport};

use fluxbridge_core::protocol::TouchEvent;
use fluxbridge_core::InputSink;

use crate::connection::ConnectionConfig;
use crate::pipeline::{ControlSinkSlot, PipelineConfig};
use crate::updater_ui::{
    run_update, spawn_update_check, update_event_is_error, update_event_status, UpdateEvent,
};
use crate::wireless_ui::{self, WirelessEvent};

/// The in-app help / "how to connect" guide shown by the Help panel.
///
/// Kept HONEST about what the code actually does today: Android mirroring over
/// USB and Wi-Fi via adb, driving the scrcpy server pushed to the device.
/// Bluetooth is deliberately NOT used for video (its bandwidth is far too low
/// for live H.264) and iOS is a documented future strategy, not yet functional.
/// See `docs/IOS_STRATEGY.md`, `android-server/README.md` and the root README.
const HELP_TEXT: &str = "\
FluxBridge mirrors and controls your Android phone from this PC over USB or Wi-Fi.

HOW IT CONNECTS (the short version)
FluxBridge talks to your phone with adb (Android Debug Bridge). It pushes the
scrcpy server to the phone, sets up a local port forward, launches the server,
then streams H.264 video back over that forwarded socket and decodes it here.

WHAT YOU NEED
- This FluxBridge desktop client (you are running it).
- adb (part of the Android platform tools). You do NOT have to install it:
  FluxBridge looks for adb on your PATH and in the usual Android SDK locations,
  and if it is missing it downloads Google's official platform-tools for you on
  first use and keeps a private copy.
- An Android phone with Developer Options and USB debugging enabled:
  Settings > About phone > tap Build number 7 times, then
  Settings > System > Developer options > USB debugging.
- Default connection target: 127.0.0.1:27183 (the forwarded port). You can
  override it by passing a host:port argument when launching the client.

CONNECT OVER USB (recommended, most reliable)
1. Plug the phone into the PC with a USB cable.
2. On the phone, accept the 'Allow USB debugging' prompt for this computer.
3. Click Start. FluxBridge runs, roughly:
     adb push scrcpy-server.jar /data/local/tmp/
     adb forward tcp:27183 localabstract:scrcpy
     adb shell CLASSPATH=/data/local/tmp/scrcpy-server.jar \\
         app_process / com.genymobile.scrcpy.Server
   then dials 127.0.0.1:27183, which adb tunnels to the phone.

CONNECT OVER WI-FI (automatic discovery + one-time pairing)
FluxBridge uses Android 11+ Wireless debugging, so you do NOT type an IP by hand.
This is a one-time setup. After the first pairing FluxBridge reconnects on its
own. Take it step by step.

Step 1. Enable Developer Options on the phone (one time).
   Open Settings > About phone and tap 'Build number' 7 times. The phone
   confirms that you are now a developer. The exact place varies by brand: on
   many Samsung phones 'Build number' lives under Settings > About phone >
   Software information. If you cannot find it, search your Settings app for
   'Build number'.

Step 2. Turn on Wireless debugging (one time).
   Open Settings > System > Developer options > Wireless debugging and switch
   it On. Keep the phone on the SAME Wi-Fi network as this PC.

Step 3. Open the pairing screen on the phone FIRST, then pair.
   On the phone, inside Wireless debugging, tap 'Pair device with pairing code'.
   The phone shows a 6-digit code and an IP address with a port. Leave that
   screen open: the code and the pairing endpoint only exist while this screen
   is on, and the PC cannot pop it up for you remotely.
   In FluxBridge, open 'Connect wirelessly' from the Start menu and click Scan.
   FluxBridge discovers the phone automatically over mDNS (it drives
   'adb mdns services' for you). For a phone you have never paired before, use
   'Pair & connect' and type the 6-digit code from the phone, NOT plain
   Connect. FluxBridge runs 'adb pair' and then 'adb connect' for you.

Step 4. After the first pairing, just launch FluxBridge.
   FluxBridge remembers the device, so the NEXT time you start it, it
   auto-reconnects with no code and no clicks. You only repeat Step 3 if you
   forget the device or reset Wireless debugging.

To end a session: click 'Stop mirroring' to disconnect and return to the Start
menu without quitting the app, then Start (or 'Connect wirelessly') again to
reconnect quickly.

Honest note: the one-time 6-digit code is shown only on the phone, and only
while the 'Pair device with pairing code' screen is open, so that very first
pairing cannot be fully automatic. Everything after it is.

WHY DOES FLUXBRIDGE NEED DEVELOPER OPTIONS / WIRELESS DEBUGGING?
Short answer: full mirroring plus control over Wi-Fi requires adb, and adb over
Wi-Fi requires Developer Options > Wireless debugging. There is no way around
this for an app of this kind. scrcpy and Vysor need exactly the same thing.
Android has no public API that lets a regular app both capture the screen and
inject touches and keys without adb.
The alternatives are weaker and out of scope for FluxBridge:
- A view-only phone-side app using MediaProjection could show the screen, but it
  cannot inject input (so no control from the PC) and it pops up a capture
  permission prompt every single session.
- Vendor tools such as Samsung DeX or Windows Phone Link work without adb, but
  they are brand-specific and do not give the same full-control mirroring.
So FluxBridge asks for a one-time Developer Options setup, and auto-reconnects
afterward, rather than shipping a weaker view-only experience.

ABOUT BLUETOOTH
Bluetooth is NOT used for video. Live phone mirroring is H.264 at tens of
megabits per second, and Bluetooth bandwidth is far too low to carry that, so a
Bluetooth video path is not possible. At most, Bluetooth could serve as a future
low-bandwidth side-channel (for example clipboard or notifications), never the
screen stream. Use USB or Wi-Fi for mirroring.

ABOUT iOS / iPhone
iOS is a documented future strategy and is NOT functional yet. Apple does not
expose an adb-style capture or input-injection API, so the planned approach
(AirPlay receiver for mirroring, libimobiledevice / WebDriverAgent for control)
is described in docs/IOS_STRATEGY.md but not implemented in this build.

UPDATES
FluxBridge checks for updates automatically at startup. You can also re-check at
any time from the Start menu using 'Check for updates'. If a newer release is
available you will be offered it; otherwise you will see a short confirmation
that you are on the latest version.

TROUBLESHOOTING
- 'adb not found': you normally do not need to do anything. The first time it is
  needed, FluxBridge downloads Google's official Android platform-tools for you
  and keeps a private copy, so adb never has to be installed or put on PATH by
  hand. If that download is blocked (no internet, a firewall, or an offline PC),
  install the Android platform tools yourself and make sure adb is on your PATH
  (or under ANDROID_HOME / ANDROID_SDK_ROOT).
- 'no device' / cannot connect: confirm the phone is plugged in and authorized
  (USB) or reachable at its IP (Wi-Fi), and that USB debugging is enabled.
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
    pipeline_cfg: PipelineConfig,
    current_version: String,
) -> Result<MainWindow, slint::PlatformError> {
    let window = MainWindow::new()?;
    window.set_connection_status(format!("Target {}", config.address()).into());

    // Set the live window icon from the embedded PNG. Done on a one-shot timer
    // so the winit window exists (the accessor only yields a window once the
    // event loop is active). Degrades to a logged no-op if anything is missing
    // or the winit backend is not in use; it never panics.
    install_window_icon(&window);

    // Seed the in-app help panel with the connection guide. It stays hidden
    // until the user clicks the Help button.
    window.set_help_text(HELP_TEXT.into());
    window.set_help_visible(false);
    // Show the landing menu on launch; mirroring starts only when the user
    // clicks Start.
    window.set_menu_visible(true);

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

    // --- Start-menu wiring -------------------------------------------------

    // Quit: cleanly stop the Slint event loop.
    window.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // Channel the decoded frames flow through once Start kicks off the pipeline.
    let (frame_tx, frame_rx) = mpsc::channel::<DecodedFrame>();
    // Channel the pipeline reports status/errors on.
    let (status_tx, status_rx) = mpsc::channel::<crate::pipeline::PipelineStatus>();
    attach_pipeline_status(&window, status_rx);
    attach_frame_sink(&window, frame_rx);

    // Shared slot the pipeline installs the scrcpy control sink into once the
    // control socket is up and the video header is parsed. The video-pointer
    // handler below takes the sink and forwards taps to the device, making
    // input injection a real, exercised path (not dead code).
    let control_slot = Arc::new(ControlSinkSlot::new());

    // Start the adb-driven pipeline on a worker thread. Shared by the Start
    // button (USB / auto-reconnect transport from the config) and by the
    // wireless panel once it has paired/connected a device (an explicit Wi-Fi
    // transport is passed in). A guard prevents spawning duplicate pipelines.
    let started = Rc::new(Cell::new(false));
    // Shared cooperative cancel flag: the UI trips it on Stop and the pipeline
    // thread observes it to break its loop and run teardown exactly once. It is
    // reset to NOT-cancelled before each spawn so a later Start runs fresh.
    let cancel = crate::pipeline::CancelFlag::new();
    let pipeline_cfg = Arc::new(pipeline_cfg);
    let start_slot = Arc::clone(&control_slot);
    let start_status_tx = status_tx.clone();
    let start_frame_tx = frame_tx.clone();
    let start_pipeline: Rc<dyn Fn(Option<crate::pipeline::Transport>)> = {
        let started = Rc::clone(&started);
        let pipeline_cfg = Arc::clone(&pipeline_cfg);
        let cancel = cancel.clone();
        Rc::new(move |transport: Option<crate::pipeline::Transport>| {
            if started.replace(true) {
                // Already running; nothing more to spawn.
                return;
            }
            // Clear any prior stop request so this fresh run is not cancelled.
            cancel.reset();
            // Clone the base config and override the transport when the wireless
            // panel supplies a freshly discovered Wi-Fi endpoint.
            let mut cfg = (*pipeline_cfg).clone();
            if let Some(t) = transport {
                cfg.transport = t;
            }
            let cfg = Arc::new(cfg);
            let status = start_status_tx.clone();
            let frames = start_frame_tx.clone();
            let slot = Arc::clone(&start_slot);
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let _ = crate::pipeline::run_pipeline_full(
                    &cfg,
                    &status,
                    Some(frames),
                    Some(slot),
                    Some(cancel),
                );
            });
        })
    };

    // Start button: hide the menu and begin mirroring with the config transport.
    let weak = window.as_weak();
    let start_for_button = Rc::clone(&start_pipeline);
    window.on_start(move || {
        let Some(w) = weak.upgrade() else { return };
        w.set_menu_visible(false);
        w.set_connection_status("Starting...".into());
        start_for_button(None);
    });

    // Stop mirroring: end the current session and return to the Start menu
    // WITHOUT quitting the app (distinct from on_quit). Tripping the shared
    // cancel flag makes the pipeline thread break its loop and run the existing
    // teardown (forward --remove, kill the server child, Wi-Fi disconnect)
    // exactly once; the thread self-exits when run_pipeline_full returns. This
    // is idempotent and a safe no-op when nothing is mirroring. The UI is reset
    // to the menu state (menu re-shown, video surface cleared) and `started` is
    // released so a later Start spawns a fresh pipeline. Remembered devices are
    // left untouched so reconnect stays quick.
    let weak = window.as_weak();
    let stop_cancel = cancel.clone();
    let stop_started = Rc::clone(&started);
    window.on_stop_mirroring(move || {
        stop_cancel.cancel();
        stop_started.set(false);
        if let Some(w) = weak.upgrade() {
            w.set_menu_visible(true);
            // Clear the video surface so the placeholder shows again. An empty
            // image has width 0, which the slint fallback keys off.
            w.set_frame(slint::Image::default());
            w.set_connection_status("Ready".into());
            w.set_pipeline_status("Idle".into());
            w.set_pipeline_status_is_error(false);
        }
    });

    // --- Wireless discovery / pairing panel wiring ------------------------

    wire_wireless_panel(&window, Rc::clone(&start_pipeline));

    // Video pointer input: scale normalized surface coordinates to device
    // pixels and forward a tap (down then up) through the scrcpy control sink.
    // The sink is taken from the slot once (after the pipeline installs it) and
    // held here; until then taps are dropped. This is the live PC -> device
    // input-injection path over the scrcpy control socket.
    let held_sink: Rc<RefCell<Option<Box<dyn InputSink>>>> = Rc::new(RefCell::new(None));
    let pointer_slot = Arc::clone(&control_slot);
    window.on_video_pointer(move |nx, ny, _pressed| {
        // Lazily take the sink the pipeline installed.
        if held_sink.borrow().is_none() {
            if let Some(sink) = pointer_slot.take() {
                *held_sink.borrow_mut() = Some(sink);
            }
        }
        let (dev_w, dev_h) = pointer_slot.resolution();
        if dev_w == 0 || dev_h == 0 {
            return;
        }
        let x = (nx.clamp(0.0, 1.0) * dev_w as f32).round() as u16;
        let y = (ny.clamp(0.0, 1.0) * dev_h as f32).round() as u16;
        if let Some(sink) = held_sink.borrow_mut().as_mut() {
            // A tap is a touch-down immediately followed by a touch-up at the
            // same point.
            let _ = sink.send_touch(TouchEvent {
                pointer_id: 0,
                x,
                y,
                action: 0, // down
                pressure: 0xFFFF,
            });
            let _ = sink.send_touch(TouchEvent {
                pointer_id: 0,
                x,
                y,
                action: 1, // up
                pressure: 0,
            });
        }
    });

    // Check for updates: run the existing updater on a worker thread and feed
    // the result into the same UpdateEvent pump the startup check uses.
    let manual_updater = Arc::clone(&updater);
    let manual_tx = worker_tx.clone();
    let manual_version = current_version;
    let weak = window.as_weak();
    window.on_check_for_updates(move || {
        if let Some(w) = weak.upgrade() {
            w.set_app_status("Checking for updates...".into());
            w.set_app_status_is_error(false);
        }
        // spawn_update_check needs an owned updater; clone the inner value.
        let updater = (*manual_updater).clone();
        spawn_update_check(updater, manual_version.clone(), manual_tx.clone());
    });

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

/// Wire the wireless discovery / pairing panel: open/close toggles, the
/// scan/pair/connect callbacks (each spawns a background worker so the UI event
/// loop never blocks on adb or the network), and a timer that drains
/// [`WirelessEvent`]s onto the panel's status line and device list. On a
/// [`WirelessEvent::Connected`] the shared `start_pipeline` closure is invoked
/// with the discovered Wi-Fi transport so mirroring begins on the just-connected
/// device, following the same worker-thread + mpsc + `slint::Timer` pattern the
/// updater uses.
fn wire_wireless_panel(
    window: &MainWindow,
    start_pipeline: Rc<dyn Fn(Option<crate::pipeline::Transport>)>,
) {
    // Open/close the panel.
    let weak = window.as_weak();
    window.on_wireless_toggle(move || {
        if let Some(w) = weak.upgrade() {
            w.set_wireless_visible(!w.get_wireless_visible());
        }
    });
    let weak = window.as_weak();
    window.on_wireless_close(move || {
        if let Some(w) = weak.upgrade() {
            w.set_wireless_visible(false);
        }
    });

    // Worker -> UI event channel, drained by the timer below.
    let (tx, rx) = mpsc::channel::<WirelessEvent>();

    // Scan for devices.
    let scan_tx = tx.clone();
    let weak = window.as_weak();
    window.on_wireless_scan(move || {
        if let Some(w) = weak.upgrade() {
            w.set_wireless_busy(true);
            w.set_wireless_status("Scanning...".into());
            w.set_wireless_status_is_error(false);
        }
        let tx = scan_tx.clone();
        std::thread::spawn(move || {
            wireless_ui::scan(&tx);
        });
    });

    // Pair a new device with the typed 6-digit code, then connect.
    let pair_tx = tx.clone();
    let weak = window.as_weak();
    window.on_wireless_pair_and_connect(move |code, id| {
        if let Some(w) = weak.upgrade() {
            w.set_wireless_busy(true);
            w.set_wireless_status("Pairing...".into());
            w.set_wireless_status_is_error(false);
        }
        let tx = pair_tx.clone();
        let code = code.to_string();
        let id = id.to_string();
        std::thread::spawn(move || {
            wireless_ui::pair_and_connect(&tx, &code, &id);
        });
    });

    // Connect an already-paired device selected from the list.
    let connect_tx = tx.clone();
    let weak = window.as_weak();
    window.on_wireless_connect_device(move |id| {
        if let Some(w) = weak.upgrade() {
            w.set_wireless_busy(true);
            w.set_wireless_status("Connecting...".into());
            w.set_wireless_status_is_error(false);
        }
        let tx = connect_tx.clone();
        let id = id.to_string();
        std::thread::spawn(move || {
            wireless_ui::connect_device(&tx, &id);
        });
    });

    // Drain wireless worker events into the panel properties on a UI-thread
    // timer, starting the pipeline when a device connects.
    let timer = Rc::new(slint::Timer::default());
    let weak = window.as_weak();
    let events = Rc::new(rx);
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(100),
        move || {
            let Some(w) = weak.upgrade() else { return };
            for event in events.try_iter() {
                match event {
                    WirelessEvent::Status(msg) => {
                        w.set_wireless_status(msg.into());
                        w.set_wireless_status_is_error(false);
                    }
                    WirelessEvent::Error(msg) => {
                        w.set_wireless_status(msg.into());
                        w.set_wireless_status_is_error(true);
                        w.set_wireless_busy(false);
                    }
                    WirelessEvent::Devices(list) => {
                        let model: Vec<DiscoveredDevice> = list
                            .into_iter()
                            .map(|d| DiscoveredDevice {
                                label: d.label.into(),
                                id: d.id.into(),
                                remembered: d.remembered,
                            })
                            .collect();
                        w.set_wireless_devices(
                            std::rc::Rc::new(slint::VecModel::from(model)).into(),
                        );
                        w.set_wireless_busy(false);
                    }
                    WirelessEvent::Connected(transport) => {
                        w.set_wireless_busy(false);
                        w.set_wireless_visible(false);
                        w.set_menu_visible(false);
                        w.set_connection_status("Starting...".into());
                        start_pipeline(Some(transport));
                    }
                }
            }
        },
    );
    std::mem::forget(timer);
}

/// Drain [`PipelineStatus`](crate::pipeline::PipelineStatus) updates from the
/// adb-driven pipeline onto the window's status line.
///
/// This is how the GUI build learns about connection progress and failures
/// WITHOUT depending on a console: the pipeline thread sends statuses on an
/// `mpsc` channel and a UI-thread timer copies the latest one into the
/// `pipeline-status` property (styling errors red). A windowed Windows build
/// has no stdout, so this channel is the only path for "adb not found" or
/// "no device" to reach the user.
pub fn attach_pipeline_status(
    window: &MainWindow,
    status_events: Receiver<crate::pipeline::PipelineStatus>,
) {
    use crate::pipeline::PipelineStatus;

    let timer = Rc::new(slint::Timer::default());
    let weak = window.as_weak();
    let events = Rc::new(status_events);
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(100),
        move || {
            let Some(w) = weak.upgrade() else { return };
            for status in events.try_iter() {
                let is_error = matches!(status, PipelineStatus::Error(_));
                w.set_pipeline_status(status.to_string().into());
                w.set_pipeline_status_is_error(is_error);
            }
        },
    );
    std::mem::forget(timer);
}

/// Drain [`DecodedFrame`]s off the decode loop and draw them into the window's
/// `frame` image property.
///
/// A UI-thread timer takes the most recent frame each tick and converts it into
/// a [`slint::Image`] via a [`slint::SharedPixelBuffer`]. Only RGBA frames of
/// the expected size are drawn; anything else (the `StubDecoder`'s `"stub"`
/// frames under a gui-without-decode build, or an empty/short buffer) is
/// ignored so the fallback placeholder stays visible and nothing panics. The
/// real pixels come from the FFmpeg `decode` build, which emits tightly packed
/// `"rgba"`.
pub fn attach_frame_sink(window: &MainWindow, frames: Receiver<DecodedFrame>) {
    let timer = Rc::new(slint::Timer::default());
    let weak = window.as_weak();
    let frames = Rc::new(frames);
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(16),
        move || {
            let Some(w) = weak.upgrade() else { return };
            // Only keep the latest frame per tick to avoid falling behind.
            if let Some(frame) = frames.try_iter().last() {
                if let Some(image) = decoded_frame_to_image(&frame) {
                    w.set_frame(image);
                }
            }
        },
    );
    std::mem::forget(timer);
}

/// Convert a [`DecodedFrame`] into a [`slint::Image`], or `None` when the frame
/// is not a drawable RGBA buffer.
///
/// Guards against the `StubDecoder` frames (`pixel_format == "stub"`, whose
/// bytes are encoded input, not pixels) and against any buffer whose length
/// does not match `width * height * 4`, so a non-RGBA or truncated frame cannot
/// panic `SharedPixelBuffer`.
fn decoded_frame_to_image(frame: &DecodedFrame) -> Option<slint::Image> {
    if frame.pixel_format != "rgba" {
        return None;
    }
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 {
        return None;
    }
    let expected = (w as usize) * (h as usize) * 4;
    if frame.data.len() != expected {
        return None;
    }
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    buffer
        .make_mut_bytes()
        .copy_from_slice(&frame.data);
    Some(slint::Image::from_rgba8(buffer))
}

/// Set the running window's icon from the embedded [`WINDOW_ICON_PNG`].
///
/// The winit window only exists once the event loop is active, so this arms a
/// single-shot timer that, on its first tick, decodes the PNG and calls winit's
/// `set_window_icon`. Every failure mode (decode error, no winit window, wrong
/// backend) is logged and ignored so a missing or unusable asset never panics
/// or crashes the GUI.
fn install_window_icon(window: &MainWindow) {
    use slint::winit_030::{winit::window::Icon, WinitWindowAccessor};

    let (rgba, w, h) = match decode_icon_rgba(WINDOW_ICON_PNG) {
        Some(parts) => parts,
        None => {
            eprintln!("fluxbridge: window icon asset could not be decoded; skipping");
            return;
        }
    };

    let timer = Rc::new(slint::Timer::default());
    let weak = window.as_weak();
    let timer_for_cb = Rc::clone(&timer);
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(50),
        move || {
            let Some(w_handle) = weak.upgrade() else {
                timer_for_cb.stop();
                return;
            };
            // Wait until the winit window is actually available.
            if !w_handle.window().has_winit_window() {
                return;
            }
            match Icon::from_rgba(rgba.clone(), w, h) {
                Ok(icon) => {
                    w_handle.window().with_winit_window(|win| {
                        win.set_window_icon(Some(icon.clone()));
                    });
                }
                Err(e) => {
                    eprintln!("fluxbridge: could not build window icon: {e}");
                }
            }
            // One-shot: stop after the first successful attempt.
            timer_for_cb.stop();
        },
    );
    // Keep the timer alive for the window's lifetime.
    std::mem::forget(timer);
}

/// Decode the embedded PNG into a tightly packed RGBA buffer plus dimensions,
/// or `None` on any decode error.
fn decode_icon_rgba(png_bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(png_bytes);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    // The embedded asset is authored as 8-bit RGBA; bail out otherwise rather
    // than guess a conversion.
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Some((buf, info.width, info.height))
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
    match &event {
        UpdateEvent::Available(info) => {
            w.set_update_version(info.version.clone().into());
            w.set_update_notes(info.notes.clone().into());
            w.set_update_progress(0.0);
            w.set_update_status("".into());
            w.set_update_status_is_error(false);
            w.set_update_available(true);
            // Clear any transient "checking..." line now that the dialog shows.
            w.set_app_status("".into());
            w.set_app_status_is_error(false);
            *current_release.borrow_mut() = Some(info.clone());
        }
        UpdateEvent::Progress(pct) => {
            w.set_update_progress(*pct as f32);
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
        UpdateEvent::UpToDate => {
            // Previously silent. Surface a visible confirmation on the general
            // status line so the manual "Check for updates" has feedback.
            if let Some(text) = update_event_status(&event) {
                w.set_app_status(text.into());
                w.set_app_status_is_error(false);
            }
        }
        UpdateEvent::Failed(msg) => {
            // Surface the failure in the dialog if a download was underway, and
            // always on the general status line so a manual check shows it.
            w.set_update_downloading(false);
            w.set_update_status(format!("Update failed: {msg}").into());
            w.set_update_status_is_error(true);
            if let Some(text) = update_event_status(&event) {
                w.set_app_status(text.into());
                w.set_app_status_is_error(update_event_is_error(&event));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // HELP_TEXT is compiled only under the `gui` feature (it lives in this
    // module, which is gated on `gui`), so this test runs when the crate is
    // tested with `--features gui`. It guards the beginner-friendly wireless
    // guide and the honest Developer Options rationale against silent drift.
    use super::HELP_TEXT;

    #[test]
    fn help_text_covers_the_wireless_guide_and_rationale() {
        // Enabling Developer Options and Wireless debugging.
        assert!(HELP_TEXT.contains("Build number"));
        assert!(HELP_TEXT.contains("Wireless debugging"));
        // Pairing via the 6-digit code, open the pairing screen first.
        assert!(HELP_TEXT.contains("Pair device with pairing code"));
        assert!(HELP_TEXT.contains("6-digit"));
        // Never-paired devices use Pair & connect, not plain Connect.
        assert!(HELP_TEXT.contains("Pair & connect"));
        // Auto-reconnect after the first pairing.
        assert!(HELP_TEXT.contains("auto-reconnect"));
        // The honest "Why Developer Options" rationale section.
        assert!(HELP_TEXT.contains("WHY DOES FLUXBRIDGE NEED DEVELOPER OPTIONS"));
        // The Stop mirroring control from FEAT-004 is mentioned.
        assert!(HELP_TEXT.contains("Stop mirroring"));
        // No em dashes anywhere in the help copy.
        assert!(!HELP_TEXT.contains('\u{2014}'));
    }
}
