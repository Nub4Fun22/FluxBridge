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
// Suppress the console window for the release Windows GUI build only. The
// headless Linux build (no `gui` feature) and debug builds are unaffected, so
// developers keep their console and stdout logging while end users of the
// shipped Windows .exe get a clean windowed app with no stray terminal.
#![cfg_attr(
    all(feature = "gui", windows, not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod adb;
mod connection;
mod decode_loop;
mod pipeline;
mod scrcpy;
mod updater_ui;

#[cfg(feature = "gui")]
mod ui;

use std::sync::mpsc;

use connection::ConnectionConfig;
use fluxbridge_updater::{GithubUpdater, UreqTransport};
use pipeline::{PipelineConfig, PipelineStatus};
use updater_ui::{spawn_update_check, GITHUB_OWNER, GITHUB_REPO};

/// The running application's version.
///
/// Resolved with [`resolve_version`]: the release build (`build.rs`) stamps the
/// git tag into `FLUXBRIDGE_BUILD_VERSION`, which takes precedence; otherwise we
/// fall back to Cargo's `CARGO_PKG_VERSION`. This is what makes a binary built
/// at tag vX.Y.Z honestly report X.Y.Z even if the workspace version in
/// Cargo.toml was not bumped, so the updater never re-offers an installed
/// release (the infinite-update-loop fix).
const CURRENT_VERSION: &str = resolve_version();

/// Pick the authoritative version string at compile time.
///
/// Precedence:
/// 1. `FLUXBRIDGE_BUILD_VERSION` when present (emitted by `build.rs` from the
///    release tag via `FLUXBRIDGE_RELEASE_VERSION`), so the shipped binary's
///    self-reported version equals the tag that built it.
/// 2. `CARGO_PKG_VERSION` otherwise (the default for every headless/developer
///    build), so `cargo build`/`cargo test` on the host stay unaffected.
const fn resolve_version() -> &'static str {
    match option_env!("FLUXBRIDGE_BUILD_VERSION") {
        Some(v) => v,
        None => env!("CARGO_PKG_VERSION"),
    }
}

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

    // Start the background update check; events flow to the UI. The result is
    // also reachable manually from the Start menu's "Check for updates".
    let (update_tx, update_rx) = mpsc::channel();
    let _check = spawn_update_check(build_updater(), CURRENT_VERSION.to_string(), update_tx);

    // Build and show the window. The window owns an updater (used for the
    // startup check pump, the manual re-check, and the download/apply flow) and
    // a pipeline config so the Start button can begin mirroring on demand. The
    // pipeline is NOT auto-started here: the landing menu stays up until the
    // user clicks Start, which keeps the menu flow clean and the headless build
    // path unchanged.
    use slint::ComponentHandle;
    let pipeline_cfg = pipeline_config_from(&config);
    let window = ui::build_main_window(
        &config,
        build_updater(),
        update_rx,
        pipeline_cfg,
        CURRENT_VERSION.to_string(),
    )?;

    window.run()?;
    Ok(())
}

/// Derive a [`PipelineConfig`] from the resolved connection target. A bare
/// default target means USB; an explicit non-loopback host means Wi-Fi.
fn pipeline_config_from(config: &ConnectionConfig) -> PipelineConfig {
    let mut cfg = PipelineConfig::default();
    if config.host != connection::DEFAULT_HOST && !config.host.is_empty() {
        cfg.transport = pipeline::Transport::WiFi {
            ip: config.host.clone(),
            port: 5555,
        };
    }
    cfg
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

    // Attempt the real adb-driven pipeline. On the headless CLI build it is
    // fine to print the statuses to stdout (this build has a console). Without
    // adb/a device this reports an Error status and returns, which is expected.
    let (status_tx, status_rx) = mpsc::channel();
    let pipeline_cfg = pipeline_config_from(&config);
    let handle = std::thread::spawn(move || start_pipeline(&pipeline_cfg, &status_tx));
    for status in status_rx {
        println!("pipeline: {status}");
    }
    match handle.join() {
        Ok(Ok(())) => println!("pipeline finished"),
        Ok(Err(e)) => println!("pipeline not started (expected with no device): {e}"),
        Err(_) => println!("pipeline thread panicked"),
    }
    Ok(())
}

/// Run the adb-driven mirroring pipeline, reporting status/errors through
/// `status`. This locates adb, pushes the scrcpy server, sets up the forward,
/// launches the server, runs the decode loop, and tears everything down on
/// exit. See [`pipeline::run_pipeline`].
fn start_pipeline(
    config: &PipelineConfig,
    status: &mpsc::Sender<PipelineStatus>,
) -> Result<(), adb::AdbError> {
    pipeline::run_pipeline(config, status)
}

/// Pure version-precedence logic, extracted so it is testable on the headless
/// host without relying on compile-time environment macros.
///
/// Mirrors [`resolve_version`]: a present-and-non-empty build-stamped version
/// (`FLUXBRIDGE_BUILD_VERSION`) wins over the Cargo package version. An absent
/// or empty stamp falls back to the Cargo version.
fn pick_version<'a>(build_version: Option<&'a str>, cargo_version: &'a str) -> &'a str {
    match build_version {
        Some(v) if !v.trim().is_empty() => v,
        _ => cargo_version,
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fluxbridge-desktop error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn build_stamp_takes_precedence_over_cargo_version() {
        // When build.rs stamped a release version, it must win so the shipped
        // binary reports the tag it was built at (fixes the update loop even if
        // Cargo.toml was not bumped).
        assert_eq!(pick_version(Some("0.4.0"), "0.1.0"), "0.4.0");
    }

    #[test]
    fn falls_back_to_cargo_version_when_no_stamp() {
        // The default headless/developer build sets no stamp and must report
        // CARGO_PKG_VERSION unchanged.
        assert_eq!(pick_version(None, "0.4.0"), "0.4.0");
    }

    #[test]
    fn empty_stamp_is_ignored() {
        // A present-but-empty env var must not blank out the version.
        assert_eq!(pick_version(Some(""), "0.4.0"), "0.4.0");
        assert_eq!(pick_version(Some("   "), "0.4.0"), "0.4.0");
    }

    #[test]
    fn resolve_version_matches_cargo_in_default_build() {
        // With no FLUXBRIDGE_BUILD_VERSION set at compile time (the test build),
        // resolve_version() must equal the Cargo package version, which is the
        // bumped workspace version.
        assert_eq!(resolve_version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(resolve_version(), "0.4.0");
    }
}
