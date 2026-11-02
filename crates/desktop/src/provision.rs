//! Automatic provisioning of the Android `adb` executable.
//!
//! A non-technical FluxBridge user should never have to install Android
//! platform-tools or edit their `PATH`. When [`crate::adb::locate_adb`] cannot
//! find `adb` on the host, this module downloads Google's official
//! platform-tools zip into a per-user application data directory, extracts it,
//! and returns the path to the bundled `adb` executable. Subsequent runs reuse
//! that already-extracted copy, so the download is one time only.
//!
//! ## Why a bundled copy, not a `PATH` edit
//!
//! The extracted adb lives at `<data_dir>/platform-tools/adb[.exe]` and is used
//! directly by its absolute path. This is deliberately preferred over mutating
//! the user's `PATH`:
//!
//! * it needs no administrator rights (the per-user data dir is always
//!   writable, unlike `Program Files` or the machine `PATH`);
//! * it is fully self-contained and leaves the system untouched, so uninstall
//!   is just deleting the data dir;
//! * the system / machine `PATH` is NEVER modified.
//!
//! Optionally adding the data dir to the per-user `HKCU` `PATH` is a possible
//! future low-risk enhancement, but it is NOT required for mirroring to work
//! and is intentionally not done here.
//!
//! ## Testability
//!
//! Everything that can be wrong is pure and unit-tested headlessly:
//! [`platform_tools_url`] (OS selection), [`provisioned_adb_path`] (fixed
//! layout), and [`extract_platform_tools`] (against a synthetic in-memory zip).
//! [`ensure_adb`] takes an injected [`fluxbridge_updater::HttpTransport`], so it
//! is tested end to end offline with a fake transport that serves a synthetic
//! zip, including the short-circuit on a second call. The LIVE download runs
//! only on the user's machine and must be verified there.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use fluxbridge_updater::HttpTransport;

/// Errors produced while auto-provisioning adb.
#[derive(Debug)]
pub enum ProvisionError {
    /// A filesystem operation failed (creating the data dir, writing the temp
    /// file, extracting an entry).
    Io(std::io::Error),
    /// The platform-tools download failed (network error, non-2xx status).
    Http(String),
    /// The downloaded archive could not be opened or extracted.
    Zip(String),
    /// The extracted archive did not contain the expected `adb` executable.
    AdbMissing {
        /// Where the adb executable was expected to be.
        expected: PathBuf,
    },
    /// The per-user data directory could not be resolved on this platform.
    NoDataDir,
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProvisionError::Io(e) => write!(f, "file error while provisioning adb: {e}"),
            ProvisionError::Http(e) => {
                write!(f, "could not download Android platform-tools: {e}")
            }
            ProvisionError::Zip(e) => {
                write!(f, "could not extract Android platform-tools: {e}")
            }
            ProvisionError::AdbMissing { expected } => write!(
                f,
                "the downloaded platform-tools did not contain adb at {}",
                expected.display()
            ),
            ProvisionError::NoDataDir => {
                write!(f, "could not determine a per-user data directory for FluxBridge")
            }
        }
    }
}

impl std::error::Error for ProvisionError {}

impl From<std::io::Error> for ProvisionError {
    fn from(e: std::io::Error) -> Self {
        ProvisionError::Io(e)
    }
}

/// Progress reported by [`ensure_adb`] so the UI can show a status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionProgress {
    /// Downloading the platform-tools zip; carries a `0..=100` percentage.
    Downloading(u8),
    /// The download finished and the archive is being extracted.
    Extracting,
    /// Provisioning is complete; carries the resolved adb path.
    Done(PathBuf),
}

/// The adb executable file name for the current OS.
fn adb_exe_name() -> &'static str {
    if cfg!(windows) {
        "adb.exe"
    } else {
        "adb"
    }
}

/// The official Google platform-tools download URL for the current OS.
///
/// Google publishes a single rolling "latest" zip per platform on
/// `dl.google.com`; FluxBridge downloads that rather than pinning a version so
/// the bundled adb stays current. All three variants are compiled in for dev
/// parity and the right one is selected via `target_os`.
pub fn platform_tools_url() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "https://dl.google.com/android/repository/platform-tools-latest-windows.zip"
    }
    #[cfg(target_os = "macos")]
    {
        "https://dl.google.com/android/repository/platform-tools-latest-darwin.zip"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "https://dl.google.com/android/repository/platform-tools-latest-linux.zip"
    }
}

/// Resolve the per-user FluxBridge application data directory.
///
/// Uses the `directories` crate so the location is the platform-native
/// per-user data dir. On Windows this lands at `%LOCALAPPDATA%\FluxBridge`,
/// which is always writable without administrator rights.
pub fn data_dir() -> Result<PathBuf, ProvisionError> {
    directories::ProjectDirs::from("", "", "FluxBridge")
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .ok_or(ProvisionError::NoDataDir)
}

/// The fixed location the bundled adb executable lives at once provisioned:
/// `<data_dir>/platform-tools/adb[.exe]`.
///
/// This is deterministic and does not touch the filesystem, so adb discovery
/// can cheaply check it first on every run.
pub fn provisioned_adb_path(data_dir: &Path) -> PathBuf {
    data_dir.join("platform-tools").join(adb_exe_name())
}

/// Extract a platform-tools zip into `dest_dir`, preserving the archive's
/// top-level `platform-tools/` subtree, and return the resolved adb path.
///
/// `archive` is any seekable reader over the zip bytes (a file or an in-memory
/// cursor). Every entry is written under `dest_dir` with its archive-relative
/// path, so a `platform-tools/adb.exe` entry lands at
/// `<dest_dir>/platform-tools/adb.exe`. On unix the adb file's executable bit
/// is set after extraction. Returns [`ProvisionError::AdbMissing`] if the
/// archive did not contain the expected adb executable.
pub fn extract_platform_tools<R: Read + Seek>(
    archive: R,
    dest_dir: &Path,
) -> Result<PathBuf, ProvisionError> {
    let mut zip = zip::ZipArchive::new(archive).map_err(|e| ProvisionError::Zip(e.to_string()))?;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| ProvisionError::Zip(e.to_string()))?;
        // Use the archive's sanitized relative path to avoid zip-slip (an entry
        // whose name escapes dest_dir via `..`). `enclosed_name` returns None
        // for a hostile path, which we skip.
        let rel = match entry.enclosed_name() {
            Some(p) => p,
            None => continue,
        };
        let out_path = dest_dir.join(rel);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out_file = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out_file)?;
        out_file.flush()?;

        // Preserve the executable bit on unix so the extracted adb (and the
        // other platform-tools binaries) can be run directly.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = std::fs::set_permissions(
                    &out_path,
                    std::fs::Permissions::from_mode(mode),
                );
            }
        }
    }

    let adb_path = provisioned_adb_path(dest_dir);
    if adb_path.is_file() {
        // Belt and suspenders on unix: ensure adb itself is executable even if
        // the archive entry carried no mode bits.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&adb_path) {
                let mut perms = meta.permissions();
                perms.set_mode(perms.mode() | 0o755);
                let _ = std::fs::set_permissions(&adb_path, perms);
            }
        }
        Ok(adb_path)
    } else {
        Err(ProvisionError::AdbMissing { expected: adb_path })
    }
}

/// Ensure a usable `adb` exists under `data_dir`, downloading and extracting
/// Google's official platform-tools if it is not already provisioned.
///
/// Behavior:
/// 1. If `<data_dir>/platform-tools/adb[.exe]` already exists it is returned
///    immediately, so the download happens at most once (and `transport` is not
///    touched on subsequent runs).
/// 2. Otherwise the platform-tools zip is downloaded into a private staging
///    directory via the injected [`HttpTransport`] (reporting
///    [`ProvisionProgress::Downloading`]), extracted there (reporting
///    [`ProvisionProgress::Extracting`]), and only once the FULL extract
///    succeeded is the staged `platform-tools/` renamed into its final place
///    under `data_dir`. The staging directory (including the downloaded zip) is
///    removed on every exit path, so a crash or failure mid-extract can never
///    leave a half-written `platform-tools/` that a later run would trust.
///
/// The transport is injected (the production caller passes the updater's
/// `UreqTransport`), so this is unit-tested fully offline with a fake transport
/// that serves a synthetic zip. The LIVE download must be verified on the
/// user's machine.
pub fn ensure_adb(
    data_dir: &Path,
    transport: &impl HttpTransport,
    progress: &mut dyn FnMut(ProvisionProgress),
) -> Result<PathBuf, ProvisionError> {
    // 1. Already provisioned: short-circuit without any network access.
    //    The install is written atomically (see below), so the presence of the
    //    adb file implies its platform-tools siblings landed too; this check is
    //    therefore a trustworthy "complete install" signal.
    let final_adb = provisioned_adb_path(data_dir);
    if final_adb.is_file() {
        progress(ProvisionProgress::Done(final_adb.clone()));
        return Ok(final_adb);
    }

    std::fs::create_dir_all(data_dir)?;

    // A private staging directory (unique per process) that everything is
    // downloaded into and extracted under. It is cleaned up on EVERY exit path
    // via the `StagingDir` guard, so a partial download or partial extract
    // never leaves an artifact behind, and the final `platform-tools/` only
    // ever appears as the result of a complete, successful extract.
    let staging = StagingDir::new(data_dir.join(format!(".staging-{}", std::process::id())))?;

    // 2. Download the official zip into the staging directory.
    let zip_path = staging.path().join("platform-tools-download.zip");
    {
        let mut on_byte = |pct: u8| progress(ProvisionProgress::Downloading(pct));
        transport
            .download_to_file(platform_tools_url(), &zip_path, &mut on_byte)
            .map_err(|e| ProvisionError::Http(e.to_string()))?;
    }

    // 3. Extract into the staging directory, producing
    //    `<staging>/platform-tools/`.
    progress(ProvisionProgress::Extracting);
    let file = std::fs::File::open(&zip_path)?;
    let staged_adb = extract_platform_tools(file, staging.path())?;

    // 4. Atomically move the fully extracted `platform-tools/` into its final
    //    place. Only after this does the short-circuit in step 1 see an install.
    let staged_platform_tools = staging.path().join("platform-tools");
    let final_platform_tools = data_dir.join("platform-tools");
    // A stale/partial `platform-tools` could exist from an older pre-atomic run;
    // clear it so the rename lands on a clean target.
    let _ = std::fs::remove_dir_all(&final_platform_tools);
    std::fs::rename(&staged_platform_tools, &final_platform_tools)?;
    debug_assert!(staged_adb.starts_with(staging.path()));

    // The staging guard drops here, removing the (now empty) staging dir and the
    // downloaded zip.
    drop(staging);

    progress(ProvisionProgress::Done(final_adb.clone()));
    Ok(final_adb)
}

/// RAII guard for the provisioning staging directory: creates it on
/// construction and recursively removes it on drop, so the downloaded zip and
/// any partially extracted tree are cleaned up on every exit path (success,
/// early `?` return, or panic).
struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    fn new(path: PathBuf) -> Result<Self, ProvisionError> {
        // Start from a clean slate in case a previous run was killed before its
        // own guard could run.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(StagingDir { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Build a synthetic platform-tools-shaped zip in memory: a
    /// `platform-tools/adb[.exe]` entry plus a sibling file, so extraction can
    /// be proven without a real Google download.
    fn synthetic_platform_tools_zip() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<'_, ()> =
                zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated);

            let adb_entry = format!("platform-tools/{}", adb_exe_name());
            zip.start_file(adb_entry, opts).unwrap();
            zip.write_all(b"#!/bin/sh\necho fake-adb\n").unwrap();

            zip.start_file("platform-tools/NOTICE.txt", opts).unwrap();
            zip.write_all(b"platform-tools license notice").unwrap();

            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    /// An in-memory [`HttpTransport`] for offline provisioning tests. It serves
    /// the provided bytes from `download_to_file` and counts how many times the
    /// download was invoked, so a test can assert a second `ensure_adb` call
    /// short-circuits without re-downloading.
    struct FakeZipTransport {
        bytes: Vec<u8>,
        download_calls: AtomicU32,
    }

    impl FakeZipTransport {
        fn new(bytes: Vec<u8>) -> Self {
            FakeZipTransport {
                bytes,
                download_calls: AtomicU32::new(0),
            }
        }

        fn download_calls(&self) -> u32 {
            self.download_calls.load(Ordering::SeqCst)
        }
    }

    impl HttpTransport for FakeZipTransport {
        fn get_text(&self, _url: &str) -> Result<String, fluxbridge_updater::UpdaterError> {
            Ok(String::new())
        }

        fn download_to_file(
            &self,
            _url: &str,
            dest: &Path,
            progress: &mut fluxbridge_updater::ProgressCallback<'_>,
        ) -> Result<(), fluxbridge_updater::UpdaterError> {
            self.download_calls.fetch_add(1, Ordering::SeqCst);
            progress(0);
            std::fs::write(dest, &self.bytes)?;
            progress(100);
            Ok(())
        }
    }

    #[test]
    fn platform_tools_url_is_official_google_host() {
        let url = platform_tools_url();
        assert!(
            url.starts_with("https://dl.google.com/android/repository/platform-tools-latest-"),
            "must be the official dl.google.com platform-tools url, got {url}"
        );
        // The variant matches the OS we compiled for.
        if cfg!(target_os = "windows") {
            assert!(url.ends_with("windows.zip"));
        } else if cfg!(target_os = "macos") {
            assert!(url.ends_with("darwin.zip"));
        } else {
            assert!(url.ends_with("linux.zip"));
        }
    }

    #[test]
    fn provisioned_adb_path_joins_platform_tools_and_os_exe() {
        let base = Path::new("/tmp/fluxbridge-data");
        let p = provisioned_adb_path(base);
        assert!(p.ends_with(Path::new("platform-tools").join(adb_exe_name())));
        // Deterministic.
        assert_eq!(p, provisioned_adb_path(base));
        if cfg!(windows) {
            assert!(p.to_string_lossy().ends_with("adb.exe"));
        } else {
            assert!(p.to_string_lossy().ends_with("adb"));
        }
    }

    #[test]
    fn extract_platform_tools_writes_adb_and_sibling() {
        let zip_bytes = synthetic_platform_tools_zip();
        let dest = std::env::temp_dir().join(format!(
            "fluxbridge-extract-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(&dest).unwrap();

        let adb_path = extract_platform_tools(Cursor::new(zip_bytes), &dest).unwrap();

        // The returned adb path exists and is under dest.
        assert!(adb_path.is_file(), "extracted adb should exist");
        assert!(adb_path.starts_with(&dest));
        assert_eq!(adb_path, provisioned_adb_path(&dest));
        // The sibling file was also extracted under platform-tools/.
        let sibling = dest.join("platform-tools").join("NOTICE.txt");
        assert!(sibling.is_file(), "sibling file should be extracted");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&adb_path).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "adb should be executable on unix");
        }

        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn ensure_adb_downloads_extracts_then_short_circuits() {
        let transport = FakeZipTransport::new(synthetic_platform_tools_zip());
        let data_dir = std::env::temp_dir().join(format!(
            "fluxbridge-ensure-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&data_dir);

        // First call: downloads, extracts, returns a working adb path.
        let mut events = Vec::new();
        let adb_path = ensure_adb(&data_dir, &transport, &mut |p| events.push(p)).unwrap();
        assert!(adb_path.is_file(), "adb should exist after provisioning");
        assert_eq!(adb_path, provisioned_adb_path(&data_dir));
        assert_eq!(transport.download_calls(), 1, "first call downloads");
        assert!(
            events.iter().any(|e| matches!(e, ProvisionProgress::Downloading(_))),
            "a Downloading progress event should be reported: {events:?}"
        );
        assert!(
            matches!(events.last(), Some(ProvisionProgress::Done(_))),
            "the last progress event should be Done: {events:?}"
        );

        // Second call: the provisioned adb already exists, so it must
        // short-circuit WITHOUT hitting the transport again.
        let adb_path2 = ensure_adb(&data_dir, &transport, &mut |_| {}).unwrap();
        assert_eq!(adb_path2, adb_path);
        assert_eq!(
            transport.download_calls(),
            1,
            "second call must not re-download"
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn ensure_adb_installs_siblings_and_leaves_no_staging_or_zip() {
        let transport = FakeZipTransport::new(synthetic_platform_tools_zip());
        let data_dir = std::env::temp_dir().join(format!(
            "fluxbridge-ensure-atomic-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&data_dir);

        let adb_path = ensure_adb(&data_dir, &transport, &mut |_| {}).unwrap();

        // The full install (adb AND its sibling) landed under the FINAL path,
        // not a staging dir, so the short-circuit can trust the adb file.
        assert_eq!(adb_path, provisioned_adb_path(&data_dir));
        assert!(adb_path.is_file(), "adb should exist at the final path");
        let sibling = data_dir.join("platform-tools").join("NOTICE.txt");
        assert!(sibling.is_file(), "sibling must be installed alongside adb");

        // No staging directory and no downloaded zip are left behind anywhere
        // under the data dir.
        for entry in std::fs::read_dir(&data_dir).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with(".staging-"),
                "staging dir must be cleaned up, found {name}"
            );
            assert!(
                !name.ends_with(".zip"),
                "downloaded zip must be cleaned up, found {name}"
            );
        }

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn ensure_adb_failed_extract_leaves_no_partial_install_or_zip() {
        // A transport that serves bytes that are NOT a valid zip, so extraction
        // fails after a successful "download".
        let transport = FakeZipTransport::new(b"this is not a zip archive".to_vec());
        let data_dir = std::env::temp_dir().join(format!(
            "fluxbridge-ensure-fail-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&data_dir);

        let result = ensure_adb(&data_dir, &transport, &mut |_| {});
        assert!(result.is_err(), "a corrupt archive must fail provisioning");
        assert_eq!(transport.download_calls(), 1, "the download was attempted");

        // The failed extract must NOT leave a (partial) platform-tools install
        // that a later run would short-circuit onto, and must NOT leave the
        // downloaded zip or the staging dir behind.
        assert!(
            !data_dir.join("platform-tools").exists(),
            "no platform-tools/ must be left after a failed extract"
        );
        if data_dir.exists() {
            for entry in std::fs::read_dir(&data_dir).unwrap() {
                let name = entry.unwrap().file_name();
                let name = name.to_string_lossy();
                assert!(
                    !name.starts_with(".staging-"),
                    "staging dir must be cleaned up on failure, found {name}"
                );
                assert!(
                    !name.ends_with(".zip"),
                    "downloaded zip must be cleaned up on failure, found {name}"
                );
            }
        }

        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
