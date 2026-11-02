//! Glue between [`fluxbridge_updater`] and the Slint `UpdateDialog`.
//!
//! The UI layer should stay thin: this module owns the update workflow and
//! exposes small, testable pieces that the Slint callbacks call into. The
//! workflow is:
//!
//! 1. On startup, [`spawn_update_check`] runs [`check_in_background`] on a
//!    worker thread so the GitHub API call never blocks the UI event loop.
//! 2. If an update is available, the app surfaces the release notes in the
//!    dialog and shows it.
//! 3. "Actualizeaza acum" invokes [`run_update`], which downloads the asset
//!    (feeding progress back through a callback bound to the progress bar) and
//!    then applies it.
//! 4. "Mai tarziu / Refuza" simply dismisses the dialog.
//!
//! All of this is pure Rust and compiles with or without the `gui` feature, so
//! the headless CI host builds and tests it. The actual wiring of these
//! functions to Slint callbacks lives in `src/ui.rs` behind `#[cfg(feature =
//! "gui")]`.

use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use fluxbridge_core::ReleaseInfo;
use fluxbridge_updater::{ApplyOutcome, GithubUpdater, HttpTransport, UpdaterResult};

/// Default GitHub owner for the FluxBridge releases.
pub const GITHUB_OWNER: &str = "Nub4Fun22";
/// Default GitHub repository name for the FluxBridge releases.
pub const GITHUB_REPO: &str = "FluxBridge";

/// A message the background update worker sends back to the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    /// A newer release is available; show the dialog with these details.
    Available(ReleaseInfo),
    /// Already up to date; nothing to show.
    UpToDate,
    /// Download progress in `0..=100`.
    Progress(u8),
    /// The installer was launched and the app must now exit so it can replace
    /// the running binary. The payload is the installer path that was spawned.
    /// The UI turns this into a `slint::quit_event_loop()`.
    Applied(String),
    /// The asset finished downloading but there is no in-app installer on this
    /// platform, so the user must apply it manually. The payload is the path of
    /// the downloaded file. The UI surfaces this to the user instead of
    /// claiming the update was applied.
    DownloadedManual(String),
    /// A step failed; the message is user-displayable.
    Failed(String),
}

/// Run the update check synchronously and return the outcome.
///
/// Separated from threading so it can be unit-tested with a fake transport.
pub fn check_in_background<T: HttpTransport>(
    updater: &GithubUpdater<T>,
    current_version: &str,
) -> UpdateEvent {
    match updater.check_for_update(current_version) {
        Ok(Some(info)) => UpdateEvent::Available(info),
        Ok(None) => UpdateEvent::UpToDate,
        Err(e) => UpdateEvent::Failed(e.to_string()),
    }
}

/// Spawn a worker thread that runs the startup update check and reports the
/// result on `events`.
///
/// Returns the thread handle so the caller can join it on shutdown if desired.
/// The `updater` is moved into the thread; it must be `Send` (its transport is).
pub fn spawn_update_check<T>(
    updater: GithubUpdater<T>,
    current_version: String,
    events: Sender<UpdateEvent>,
) -> JoinHandle<()>
where
    T: HttpTransport + 'static,
{
    std::thread::Builder::new()
        .name("fluxbridge-update-check".into())
        .spawn(move || {
            let event = check_in_background(&updater, &current_version);
            let _ = events.send(event);
        })
        .expect("failed to spawn update-check thread")
}

/// Download and apply the update, streaming progress to `events`.
///
/// Sends [`UpdateEvent::Progress`] values as the download proceeds, then a
/// terminal event that honestly reflects what [`GithubUpdater::apply_update`]
/// did:
///
/// * [`UpdateEvent::Applied`] when the installer was launched and the app must
///   now exit (Windows). The UI reacts by quitting the event loop.
/// * [`UpdateEvent::DownloadedManual`] when the asset was downloaded but no
///   in-app installer ran (the non-Windows portable stub), so the user has to
///   apply it by hand.
///
/// A failure at any step propagates as `Err`; the caller maps it to
/// [`UpdateEvent::Failed`]. The destination file is placed in the system temp
/// directory.
pub fn run_update<T: HttpTransport>(
    updater: &GithubUpdater<T>,
    release: &ReleaseInfo,
    events: &Sender<UpdateEvent>,
) -> UpdaterResult<String> {
    let file_name = release
        .asset_url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("fluxbridge-update.bin");
    let dest = std::env::temp_dir().join(file_name);

    let events_cb = events.clone();
    let mut progress = move |pct: u8| {
        let _ = events_cb.send(UpdateEvent::Progress(pct));
    };
    updater.download_asset(&release.asset_url, &dest, &mut progress)?;
    let outcome = updater.apply_update(&dest)?;

    let path = dest.to_string_lossy().into_owned();
    let terminal = match outcome {
        ApplyOutcome::ExitForInstaller => UpdateEvent::Applied(path.clone()),
        ApplyOutcome::DownloadedApplyManually => UpdateEvent::DownloadedManual(path.clone()),
    };
    let _ = events.send(terminal);
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    // Reuse the updater crate's public API with a tiny local fake transport so
    // this module's glue is tested without network or the real transport.
    #[derive(Clone)]
    struct LocalFake {
        text: String,
        bytes: Vec<u8>,
    }

    impl HttpTransport for LocalFake {
        fn get_text(&self, _url: &str) -> UpdaterResult<String> {
            Ok(self.text.clone())
        }

        fn download_to_file(
            &self,
            _url: &str,
            dest: &std::path::Path,
            progress: &mut fluxbridge_updater::ProgressCallback<'_>,
        ) -> UpdaterResult<()> {
            // Mirror the real transport's progress contract: 0 then 100.
            progress(0);
            std::fs::write(dest, &self.bytes)?;
            progress(100);
            Ok(())
        }
    }

    fn fake_release_json(tag: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","body":"notes","draft":false,"assets":[{{"name":"app.exe","browser_download_url":"https://example.com/app.exe"}}]}}"#
        )
    }

    #[test]
    fn check_reports_available_when_newer() {
        let transport = LocalFake {
            text: fake_release_json("v2.0.0"),
            bytes: vec![],
        };
        let updater = GithubUpdater::new(transport, GITHUB_OWNER, GITHUB_REPO);
        let event = check_in_background(&updater, "1.0.0");
        match event {
            UpdateEvent::Available(info) => assert_eq!(info.version, "v2.0.0"),
            other => panic!("expected Available, got {other:?}"),
        }
    }

    #[test]
    fn check_reports_up_to_date() {
        let transport = LocalFake {
            text: fake_release_json("v1.0.0"),
            bytes: vec![],
        };
        let updater = GithubUpdater::new(transport, GITHUB_OWNER, GITHUB_REPO);
        assert_eq!(
            check_in_background(&updater, "1.0.0"),
            UpdateEvent::UpToDate
        );
    }

    #[test]
    fn run_update_streams_progress_and_downloads() {
        let transport = LocalFake {
            text: fake_release_json("v2.0.0"),
            bytes: vec![1, 2, 3, 4],
        };
        let updater = GithubUpdater::new(transport, GITHUB_OWNER, GITHUB_REPO);
        let release = ReleaseInfo {
            version: "v2.0.0".into(),
            notes: "notes".into(),
            asset_url: "https://example.com/app.exe".into(),
        };
        let (tx, rx) = mpsc::channel();
        let path = run_update(&updater, &release, &tx).unwrap();
        assert!(path.ends_with("app.exe"));

        let events: Vec<UpdateEvent> = rx.try_iter().collect();
        assert!(events.contains(&UpdateEvent::Progress(0)));
        assert!(events.contains(&UpdateEvent::Progress(100)));
        // On this non-Windows host apply is a portable stub, so the terminal
        // event must honestly report "downloaded, apply manually" rather than a
        // success. The Windows path emits Applied instead (not exercised here).
        assert!(
            events.iter().any(|e| matches!(
                e,
                UpdateEvent::DownloadedManual(_) | UpdateEvent::Applied(_)
            )),
            "expected a terminal apply event, got {events:?}"
        );
        #[cfg(not(windows))]
        assert!(events
            .iter()
            .any(|e| matches!(e, UpdateEvent::DownloadedManual(_))));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn spawn_update_check_sends_event() {
        let transport = LocalFake {
            text: fake_release_json("v3.0.0"),
            bytes: vec![],
        };
        let updater = GithubUpdater::new(transport, GITHUB_OWNER, GITHUB_REPO);
        let (tx, rx) = mpsc::channel();
        let handle = spawn_update_check(updater, "1.0.0".into(), tx);
        handle.join().unwrap();
        match rx.recv().unwrap() {
            UpdateEvent::Available(info) => assert_eq!(info.version, "v3.0.0"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
