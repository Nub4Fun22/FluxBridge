//! FluxBridge desktop client entry point.
//!
//! Wires the scaffold together:
//!
//! * parses an optional `host:port` target (default `127.0.0.1:27183`);
//! * kicks off the GitHub Releases update check on a background thread;
//! * starts the connection skeleton and the stream decode loop;
//! * runs the Slint event loop when built with `--features gui`.
//!
//! On the headless CI host the crate is built WITHOUT the `gui` feature, so the
//! pure-Rust pipeline is assembled and the program runs a short headless
//! sequence instead of opening a window. All the GUI-independent logic
//! (connection, decode loop, updater glue) lives in its own modules and is unit
//! tested. See `README.md` for why Slint is gated behind a feature.

// This is a scaffold binary. Several public items are intentional
// forward-looking API that later milestones wire up: the reconnect loop and
// mDNS auto-pairing hook (Phase 2), outbound packet `send`, and (in the
// headless build) the full update download/apply flow which the Slint UI drives
// only under the `gui` feature. They are all exercised by the unit tests. Allow
// dead_code so both the default and `gui` builds stay warning-clean without
// deleting documented API the roadmap depends on.
#![allow(dead_code)]

mod connection;
mod decode_loop;
mod updater_ui;

#[cfg(feature = "gui")]
mod ui;

use std::sync::mpsc;

use connection::ConnectionConfig;
use fluxbridge_updater::{GithubUpdater, UreqTransport};
use updater_ui::{spawn_update_check, GITHUB_OWNER, GITHUB_REPO};

/// The running application's version, from Cargo at build time.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Resolve the connection target from the first CLI argument, defaulting to the
/// local adb-forward address.
fn resolve_target() -> ConnectionConfig {
    match std::env::args().nth(1) {
        Some(target) => ConnectionConfig::from_target(&target),
        None => ConnectionConfig::default(),
    }
}

/// Build the production updater (ureq + rustls transport).
fn build_updater() -> GithubUpdater<UreqTransport> {
    GithubUpdater::new(UreqTransport::default(), GITHUB_OWNER, GITHUB_REPO)
}

#[cfg(feature = "gui")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = resolve_target();

    // Start the background update check; events flow to the UI.
    let (update_tx, update_rx) = mpsc::channel();
    let _check = spawn_update_check(build_updater(), CURRENT_VERSION.to_string(), update_tx);

    // Build and show the window; it pumps update events via a timer and owns an
    // updater used to download + apply when the user accepts.
    use slint::ComponentHandle;
    let window = ui::build_main_window(&config, build_updater(), update_rx)?;

    // Kick off the connection + decode loop on a background thread. Failures to
    // connect are expected in a scaffold with no server running, so we log and
    // continue showing the UI.
    std::thread::spawn(move || {
        if let Err(e) = start_pipeline(&config) {
            eprintln!("fluxbridge: pipeline not started: {e}");
        }
    });

    window.run()?;
    Ok(())
}

#[cfg(not(feature = "gui"))]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = resolve_target();
    println!("FluxBridge desktop {CURRENT_VERSION} (headless build, no GUI feature)");
    println!("target: {}", config.address());

    // Run the startup update check synchronously and report the outcome.
    let (update_tx, update_rx) = mpsc::channel();
    let handle = spawn_update_check(build_updater(), CURRENT_VERSION.to_string(), update_tx);
    let _ = handle.join();
    match update_rx.try_recv() {
        Ok(event) => println!("update check: {event:?}"),
        Err(_) => println!("update check: no result (offline is fine for the scaffold)"),
    }

    // Attempt to start the pipeline; without a server this returns an error,
    // which is expected and not fatal for the headless scaffold.
    match start_pipeline(&config) {
        Ok(()) => println!("pipeline started"),
        Err(e) => println!("pipeline not started (no server running): {e}"),
    }
    Ok(())
}

/// Connect and run the decode loop, pushing decoded frames to a sink.
///
/// In the full app the frame sink feeds the UI's video surface. Here it returns
/// as soon as the connection or decode loop ends.
fn start_pipeline(config: &ConnectionConfig) -> Result<(), Box<dyn std::error::Error>> {
    let connection = connection::Connection::connect(config)?;
    let (frame_tx, _frame_rx) = mpsc::channel();
    let mut decoder = decode_loop::StubDecoder::new();
    decode_loop::run_decode_loop(connection.packets(), &mut decoder, &frame_tx)?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fluxbridge-desktop error: {e}");
        std::process::exit(1);
    }
}
