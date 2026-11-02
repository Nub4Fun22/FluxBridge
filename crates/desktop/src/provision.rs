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
/// 2. Otherwise the platform-tools zip is downloaded to a temp file via the
///    injected [`HttpTransport`] (reporting [`ProvisionProgress::Downloading`]),
///    then extracted into `data_dir` (reporting [`ProvisionProgress::Extracting`]),
///    and the resolved adb path is returned (reporting
///    [`ProvisionProgress::Done`]).
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
    let existing = provisioned_adb_path(data_dir);
    if existing.is_file() {
        progress(ProvisionProgress::Done(existing.clone()));
        return Ok(existing);
    }

    std::fs::create_dir_all(data_dir)?;

    // 2. Download the official zip to a temp file next to the data dir, so the
    //    partial download never shadows the final platform-tools location.
    let zip_path = data_dir.join("platform-tools-download.zip");
    {
        let mut on_byte = |pct: u8| progress(ProvisionProgress::Downloading(pct));
        transport
            .download_to_file(platform_tools_url(), &zip_path, &mut on_byte)
            .map_err(|e| ProvisionError::Http(e.to_string()))?;
    }

    // 3. Extract into the data dir.
    progress(ProvisionProgress::Extracting);
    let file = std::fs::File::open(&zip_path)?;
    let adb_path = extract_platform_tools(file, data_dir)?;

    // The archive is no longer needed once extracted; a failure to remove it is
    // not fatal to provisioning.
    let _ = std::fs::remove_file(&zip_path);

    progress(ProvisionProgress::Done(adb_path.clone()));
    Ok(adb_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Cursor;

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
        download_calls: RefCell<u32>,
    }

    impl FakeZipTransport {
        fn new(bytes: Vec<u8>) -> Self {
            FakeZipTransport {
                bytes,
                download_calls: RefCell::new(0),
            }
        }
    }

    // The real transport is Send + Sync; ensure_adb only needs a shared
    // reference and our test is single-threaded, so the RefCell is safe here.
    unsafe impl Sync for FakeZipTransport {}

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
            *self.download_calls.borrow_mut() += 1;
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
        assert_eq!(*transport.download_calls.borrow(), 1, "first call downloads");
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
            *transport.download_calls.borrow(),
            1,
            "second call must not re-download"
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
