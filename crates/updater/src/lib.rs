//! # fluxbridge-updater
//!
//! The in-app updater for [FluxBridge](https://github.com/Nub4Fun22/FluxBridge).
//! It checks the project's GitHub Releases for a newer version, downloads the
//! platform asset with progress reporting, and applies the update by launching
//! the downloaded installer and scheduling an application restart.
//!
//! ## Design
//!
//! * HTTP is hidden behind the [`HttpTransport`] trait so the GitHub logic is
//!   fully unit-testable offline (see [`transport::FakeTransport`] in the test
//!   suite). The production transport, [`UreqTransport`], uses `ureq` +
//!   `rustls` so nothing links to the system OpenSSL.
//! * Version comparison uses [`semver`]; the leading `v` of a git tag is
//!   stripped before parsing.
//! * The crate implements the [`fluxbridge_core::Updater`] trait via
//!   [`GithubUpdater`] so the desktop app can depend on the core abstraction.
//!
//! ## Example
//!
//! ```no_run
//! use fluxbridge_updater::{GithubUpdater, transport::UreqTransport};
//!
//! let updater = GithubUpdater::new(
//!     UreqTransport::default(),
//!     "fluxbridge",
//!     "fluxbridge",
//! );
//! if let Some(release) = updater.check_for_update(env!("CARGO_PKG_VERSION")).unwrap() {
//!     println!("update available: {}", release.version);
//! }
//! ```

#![warn(missing_docs)]

pub mod error;
pub mod github;
pub mod transport;

pub use error::{UpdaterError, UpdaterResult};
pub use github::{GithubAsset, GithubRelease};
pub use transport::{HttpTransport, ProgressCallback, UreqTransport};

use fluxbridge_core::{CoreError, CoreResult, ReleaseInfo, Updater};
use semver::Version;
use std::path::Path;

/// The result of [`GithubUpdater::apply_update`].
///
/// `apply_update` cannot itself tear down the process (it has no handle on the
/// UI event loop), so it reports what the caller must do next. This keeps the
/// desktop UI honest: it only claims a successful update when the installer was
/// actually launched, and tells the user to apply manually on platforms that
/// have no in-app installer yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The platform installer was spawned and the application must now exit so
    /// the installer can replace the running binary. On Windows the caller is
    /// expected to request `slint::quit_event_loop()` (or otherwise exit) as
    /// soon as it sees this.
    ExitForInstaller,
    /// No in-app installer ran: the asset was downloaded but the user has to
    /// apply it by hand (the current non-Windows portable stub). The UI must
    /// surface this instead of pretending the update was applied.
    DownloadedApplyManually,
}

/// Parse a version string into a [`semver::Version`], stripping a leading `v`.
fn parse_version(raw: &str) -> UpdaterResult<Version> {
    let trimmed = raw.trim();
    let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
    Version::parse(stripped).map_err(|source| UpdaterError::Version {
        value: raw.to_string(),
        source,
    })
}

/// A concrete GitHub Releases updater.
///
/// `T` is the injected [`HttpTransport`]; production code uses
/// [`UreqTransport`], tests use a fake. The updater is cheap to clone when the
/// transport is.
#[derive(Debug, Clone)]
pub struct GithubUpdater<T: HttpTransport> {
    transport: T,
    owner: String,
    repo: String,
}

impl<T: HttpTransport> GithubUpdater<T> {
    /// Create an updater for `owner`/`repo` using `transport`.
    pub fn new(transport: T, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        GithubUpdater {
            transport,
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    /// Fetch and parse the latest release from the GitHub API.
    pub fn fetch_latest(&self) -> UpdaterResult<GithubRelease> {
        let url = GithubRelease::latest_url(&self.owner, &self.repo);
        let body = self.transport.get_text(&url)?;
        let release: GithubRelease = serde_json::from_str(&body)?;
        Ok(release)
    }

    /// Check whether a newer release than `current_version` is available.
    ///
    /// Returns `Ok(Some(ReleaseInfo))` only when the latest published release
    /// has a strictly greater semver than `current_version` and carries a
    /// downloadable asset for this platform. Returns `Ok(None)` when the
    /// current version is up to date, when the release is a draft or a
    /// pre-release, or when there is no suitable asset.
    ///
    /// Draft and pre-release builds are skipped by default so an unfinished or
    /// pre-release build is never offered as a stable update. The
    /// `releases/latest` endpoint already excludes both, but `fetch_latest` is
    /// a thin wrapper a future caller could repoint at `/releases` or a tag, so
    /// the guard is enforced here regardless of which release is returned.
    pub fn check_for_update(&self, current_version: &str) -> UpdaterResult<Option<ReleaseInfo>> {
        let current = parse_version(current_version)?;
        let release = self.fetch_latest()?;
        if release.draft || release.prerelease {
            return Ok(None);
        }
        let remote = parse_version(&release.tag_name)?;
        if remote <= current {
            return Ok(None);
        }
        let asset = match release.select_asset() {
            Some(asset) => asset,
            None => return Ok(None),
        };
        Ok(Some(ReleaseInfo {
            version: release.tag_name.clone(),
            notes: release.body.clone(),
            asset_url: asset.browser_download_url.clone(),
        }))
    }

    /// Download the asset at `url` to `dest`, reporting `0..=100` progress.
    ///
    /// The `progress` callback is invoked with monotonically increasing integer
    /// percentages. When the server does not report a content length the
    /// callback receives `0` at the start and `100` on completion.
    pub fn download_asset(
        &self,
        url: &str,
        dest: &Path,
        progress: &mut ProgressCallback<'_>,
    ) -> UpdaterResult<()> {
        self.transport.download_to_file(url, dest, progress)
    }

    /// Apply a previously downloaded update located at `installer_path`.
    ///
    /// On Windows this launches the installer (the equivalent of a
    /// `ShellExecute`/`CreateProcess` on the `.exe`/`.msi`) detached from the
    /// current process and returns [`ApplyOutcome::ExitForInstaller`]. The
    /// caller MUST then exit the app (a running `.exe` cannot be overwritten by
    /// its own installer), which lets the installer replace the binary and
    /// relaunch it. `apply_update` cannot exit the process itself because it
    /// holds no handle on the UI event loop; the desktop accept handler turns
    /// this outcome into a `slint::quit_event_loop()`.
    ///
    /// On non-Windows hosts there is no native installer to run in this
    /// scaffold, so this is a documented portable stub: it verifies the
    /// downloaded file exists and returns
    /// [`ApplyOutcome::DownloadedApplyManually`] without spawning anything or
    /// claiming the update was applied. The real Linux/macOS implementation
    /// will unpack the archive or run the platform package manager here and
    /// then return [`ApplyOutcome::ExitForInstaller`].
    pub fn apply_update(&self, installer_path: &Path) -> UpdaterResult<ApplyOutcome> {
        if !installer_path.exists() {
            return Err(UpdaterError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("installer not found at {}", installer_path.display()),
            )));
        }
        #[cfg(windows)]
        {
            // Launch the installer detached; it will replace this binary and
            // relaunch the app. The caller must now exit so the file is not
            // locked, which we signal with ExitForInstaller.
            //
            // CREATE_NO_WINDOW (0x08000000) suppresses the console window a
            // spawned child would otherwise flash on screen. The installer is a
            // GUI Inno Setup wizard, so this only hides a stray console, not the
            // installer's own window.
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            std::process::Command::new(installer_path)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?;
            Ok(ApplyOutcome::ExitForInstaller)
        }
        #[cfg(not(windows))]
        {
            // Portable stub: a real Unix build would, for example, run the
            // AppImage/`.deb`/`.dmg` installer or swap the binary in place and
            // then return ExitForInstaller. Until then we are explicit that the
            // user must apply the download manually rather than report success.
            let _ = installer_path;
            Ok(ApplyOutcome::DownloadedApplyManually)
        }
    }
}

/// Implement the core [`Updater`] trait so the desktop app can depend on the
/// abstraction defined in `fluxbridge-core`.
///
/// Downloads are written to a temporary file derived from the asset URL. The
/// `download` method here reports no progress (the trait signature has no
/// callback); callers that want a progress bar should use
/// [`GithubUpdater::download_asset`] directly, which the desktop UI does.
impl<T: HttpTransport> Updater for GithubUpdater<T> {
    fn check(&self, current_version: &str) -> CoreResult<Option<ReleaseInfo>> {
        self.check_for_update(current_version)
            .map_err(|e| CoreError::Other(e.to_string()))
    }

    fn download(&self, release: &ReleaseInfo) -> CoreResult<String> {
        let file_name = release
            .asset_url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("fluxbridge-update.bin");
        let dest = std::env::temp_dir().join(file_name);
        let mut noop: Box<ProgressCallback<'_>> = Box::new(|_pct| {});
        self.download_asset(&release.asset_url, &dest, noop.as_mut())
            .map_err(|e| CoreError::Other(e.to_string()))?;
        Ok(dest.to_string_lossy().into_owned())
    }

    fn apply(&self, downloaded_path: &str) -> CoreResult<()> {
        // The core trait has no richer return type; callers that need to know
        // whether a process exit is required use `apply_update` directly (the
        // desktop UI does). Here we just surface any error.
        self.apply_update(Path::new(downloaded_path))
            .map(|_| ())
            .map_err(|e| CoreError::Other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::tests::FakeTransport;
    use std::sync::{Arc, Mutex};

    /// A sample GitHub `releases/latest` payload with two assets.
    fn sample_release_json(tag: &str) -> String {
        let template = r#"{
                "tag_name": "@TAG@",
                "name": "FluxBridge @TAG@",
                "body": "What is new\n- Faster mirroring\n- Bug fixes",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {
                        "name": "FluxBridge-portable.AppImage",
                        "browser_download_url": "https://example.com/dl/FluxBridge.AppImage"
                    },
                    {
                        "name": "FluxBridge-Setup-@TAG@.exe",
                        "browser_download_url": "https://example.com/dl/FluxBridge-Setup.exe"
                    }
                ]
            }"#;
        template.replace("@TAG@", tag)
    }

    fn updater_for(json: String) -> GithubUpdater<FakeTransport> {
        GithubUpdater::new(FakeTransport::with_text(json), "fluxbridge", "fluxbridge")
    }

    #[test]
    fn newer_remote_returns_some() {
        let updater = updater_for(sample_release_json("v1.5.0"));
        let result = updater.check_for_update("1.0.0").unwrap();
        let info = result.expect("expected an update to be available");
        assert_eq!(info.version, "v1.5.0");
        assert!(info.notes.contains("What is new"));
    }

    #[test]
    fn equal_version_returns_none() {
        let updater = updater_for(sample_release_json("v1.0.0"));
        assert!(updater.check_for_update("1.0.0").unwrap().is_none());
    }

    #[test]
    fn older_remote_returns_none() {
        let updater = updater_for(sample_release_json("v0.9.0"));
        assert!(updater.check_for_update("1.0.0").unwrap().is_none());
    }

    #[test]
    fn leading_v_is_stripped_on_both_sides() {
        let updater = updater_for(sample_release_json("v2.0.0"));
        // current version supplied without the leading v, tag with it
        assert!(updater.check_for_update("v1.9.9").unwrap().is_some());
    }

    #[test]
    fn selects_exe_asset() {
        let updater = updater_for(sample_release_json("v1.5.0"));
        let info = updater.check_for_update("1.0.0").unwrap().unwrap();
        assert_eq!(info.asset_url, "https://example.com/dl/FluxBridge-Setup.exe");
    }

    #[test]
    fn malformed_json_yields_error() {
        let updater = updater_for("{ this is not valid json ".to_string());
        let err = updater.check_for_update("1.0.0").unwrap_err();
        assert!(matches!(err, UpdaterError::Parse(_)));
    }

    #[test]
    fn draft_release_is_ignored() {
        let json = r#"{"tag_name":"v9.9.9","body":"","draft":true,"assets":[]}"#.to_string();
        let updater = updater_for(json);
        assert!(updater.check_for_update("1.0.0").unwrap().is_none());
    }

    #[test]
    fn prerelease_is_ignored() {
        // A newer tag that is flagged as a pre-release must not be offered as a
        // stable update, mirroring the draft handling.
        let json = r#"{
            "tag_name": "v9.9.9",
            "body": "beta notes",
            "draft": false,
            "prerelease": true,
            "assets": [
                {
                    "name": "FluxBridge-Setup-v9.9.9.exe",
                    "browser_download_url": "https://example.com/dl/FluxBridge-Setup.exe"
                }
            ]
        }"#
        .to_string();
        let updater = updater_for(json);
        assert!(
            updater.check_for_update("1.0.0").unwrap().is_none(),
            "a newer pre-release must not be offered as a stable update"
        );
    }

    #[test]
    fn release_without_assets_returns_none() {
        let json = r#"{"tag_name":"v9.9.9","body":"notes","draft":false,"assets":[]}"#.to_string();
        let updater = updater_for(json);
        assert!(updater.check_for_update("1.0.0").unwrap().is_none());
    }

    #[test]
    fn invalid_current_version_errors() {
        let updater = updater_for(sample_release_json("v1.5.0"));
        let err = updater.check_for_update("not-a-version").unwrap_err();
        assert!(matches!(err, UpdaterError::Version { .. }));
    }

    #[test]
    fn download_reports_progress_zero_to_hundred() {
        let payload = vec![7u8; 200 * 1024]; // 200 KiB -> multiple chunks
        let transport = FakeTransport::with_download(payload.clone());
        let updater = GithubUpdater::new(transport, "fluxbridge", "fluxbridge");

        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let seen_cb = Arc::clone(&seen);
        let dir = std::env::temp_dir().join("fluxbridge-updater-test");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("asset.bin");
        let mut cb = move |pct: u8| seen_cb.lock().unwrap().push(pct);

        updater
            .download_asset("https://example.com/dl/asset.bin", &dest, &mut cb)
            .unwrap();

        let progress = seen.lock().unwrap().clone();
        assert_eq!(*progress.first().unwrap(), 0, "first callback should be 0");
        assert_eq!(*progress.last().unwrap(), 100, "last callback should be 100");
        // Monotonically non-decreasing.
        assert!(progress.windows(2).all(|w| w[0] <= w[1]));
        // File was written with the full payload.
        let written = std::fs::read(&dest).unwrap();
        assert_eq!(written.len(), payload.len());
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn apply_update_missing_file_errors() {
        let updater = updater_for(sample_release_json("v1.5.0"));
        let missing = std::env::temp_dir().join("fluxbridge-does-not-exist.bin");
        let _ = std::fs::remove_file(&missing);
        let err = updater.apply_update(&missing).unwrap_err();
        assert!(matches!(err, UpdaterError::Io(_)));
    }

    #[cfg(not(windows))]
    #[test]
    fn apply_update_non_windows_reports_manual() {
        // On the headless host apply is a portable stub: it must report that
        // the user has to apply the download manually rather than claim the
        // update was applied.
        let updater = updater_for(sample_release_json("v1.5.0"));
        let dir = std::env::temp_dir().join("fluxbridge-apply-manual-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("asset.bin");
        std::fs::write(&file, b"payload").unwrap();
        let outcome = updater.apply_update(&file).unwrap();
        assert_eq!(outcome, ApplyOutcome::DownloadedApplyManually);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn core_updater_trait_check_works() {
        let updater = updater_for(sample_release_json("v1.5.0"));
        // Exercise through the core trait object.
        let dyn_updater: &dyn Updater = &updater;
        let info = dyn_updater.check("1.0.0").unwrap().unwrap();
        assert_eq!(info.version, "v1.5.0");
    }
}
