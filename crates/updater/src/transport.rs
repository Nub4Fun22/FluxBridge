//! HTTP transport abstraction for the updater.
//!
//! The updater never talks to a concrete HTTP client directly. Instead it
//! depends on the small [`HttpTransport`] trait, which has exactly two
//! operations: fetch a text/JSON body (the GitHub API call) and stream a
//! binary download while reporting progress. This keeps the GitHub Releases
//! logic unit-testable offline: tests inject a [`FakeTransport`] that returns
//! canned JSON and in-memory bytes, so no network is required.
//!
//! The production implementation, [`UreqTransport`], uses [`ureq`], which is a
//! blocking client built on `rustls`. Nothing in this stack links to the system
//! OpenSSL, which is not guaranteed to be present on the headless Linux host.

use std::io::Read;

use crate::error::UpdaterError;

/// A callback invoked during a download with an integer percentage in `0..=100`.
pub type ProgressCallback<'a> = dyn FnMut(u8) + 'a;

/// Abstraction over the HTTP operations the updater needs.
///
/// Implementors must be `Send + Sync` so the updater can be shared across the
/// UI thread and a background worker thread.
pub trait HttpTransport: Send + Sync {
    /// Perform a `GET` and return the response body as a UTF-8 string.
    ///
    /// Implementations should send any headers GitHub requires (a `User-Agent`
    /// and the REST API `Accept` header).
    fn get_text(&self, url: &str) -> Result<String, UpdaterError>;

    /// Download `url` to `dest` on disk, invoking `progress` with a value in
    /// `0..=100` as bytes arrive. When the total size is unknown, progress may
    /// jump straight to `100` on completion.
    fn download_to_file(
        &self,
        url: &str,
        dest: &std::path::Path,
        progress: &mut ProgressCallback<'_>,
    ) -> Result<(), UpdaterError>;
}

/// Buffer size used when streaming a download body to disk.
const DOWNLOAD_CHUNK: usize = 64 * 1024;

/// Compute a clamped `0..=100` percentage from `read`/`total` byte counts.
///
/// Returns `None` when `total` is `0` (unknown length), so callers can decide
/// how to report indeterminate progress.
pub(crate) fn percent(read: u64, total: u64) -> Option<u8> {
    if total == 0 {
        return None;
    }
    let pct = (read.saturating_mul(100) / total).min(100);
    Some(pct as u8)
}

/// Stream `reader` into `writer`, reporting `0..=100` progress against
/// `total_len` (0 means unknown). Shared by the real and fake transports so the
/// progress semantics are identical in tests and in production.
pub(crate) fn stream_with_progress<R: Read, W: std::io::Write>(
    mut reader: R,
    writer: &mut W,
    total_len: u64,
    progress: &mut ProgressCallback<'_>,
) -> Result<(), UpdaterError> {
    let mut buf = vec![0u8; DOWNLOAD_CHUNK];
    let mut read_total: u64 = 0;
    let mut last_pct: i16 = -1;
    progress(0);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        read_total += n as u64;
        if let Some(pct) = percent(read_total, total_len) {
            if i16::from(pct) != last_pct {
                last_pct = i16::from(pct);
                progress(pct);
            }
        }
    }
    writer.flush()?;
    if last_pct != 100 {
        progress(100);
    }
    Ok(())
}

/// Production [`HttpTransport`] backed by [`ureq`] (blocking, rustls).
#[derive(Debug, Clone)]
pub struct UreqTransport {
    agent: ureq::Agent,
    user_agent: String,
}

impl UreqTransport {
    /// Create a transport that identifies itself with `user_agent` (GitHub
    /// rejects API requests without a `User-Agent` header).
    pub fn new(user_agent: impl Into<String>) -> Self {
        UreqTransport {
            agent: ureq::AgentBuilder::new().build(),
            user_agent: user_agent.into(),
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        UreqTransport::new(concat!("fluxbridge-updater/", env!("CARGO_PKG_VERSION")))
    }
}

impl HttpTransport for UreqTransport {
    fn get_text(&self, url: &str) -> Result<String, UpdaterError> {
        let response = self
            .agent
            .get(url)
            .set("User-Agent", &self.user_agent)
            .set("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| UpdaterError::Http(e.to_string()))?;
        response
            .into_string()
            .map_err(|e| UpdaterError::Http(e.to_string()))
    }

    fn download_to_file(
        &self,
        url: &str,
        dest: &std::path::Path,
        progress: &mut ProgressCallback<'_>,
    ) -> Result<(), UpdaterError> {
        let response = self
            .agent
            .get(url)
            .set("User-Agent", &self.user_agent)
            .call()
            .map_err(|e| UpdaterError::Http(e.to_string()))?;
        let total_len = response
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let reader = response.into_reader();
        let mut file = std::fs::File::create(dest)?;
        stream_with_progress(reader, &mut file, total_len, progress)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An in-memory [`HttpTransport`] for offline unit tests.
    ///
    /// `text` is returned verbatim from [`HttpTransport::get_text`] (the canned
    /// GitHub JSON) and `download` holds the bytes served by
    /// [`HttpTransport::download_to_file`].
    #[derive(Debug, Clone, Default)]
    pub(crate) struct FakeTransport {
        text: String,
        download: Vec<u8>,
    }

    impl FakeTransport {
        /// Build a transport whose `get_text` returns `text`.
        pub(crate) fn with_text(text: String) -> Self {
            FakeTransport {
                text,
                download: Vec::new(),
            }
        }

        /// Build a transport whose `download_to_file` streams `bytes`.
        pub(crate) fn with_download(bytes: Vec<u8>) -> Self {
            FakeTransport {
                text: String::new(),
                download: bytes,
            }
        }
    }

    impl HttpTransport for FakeTransport {
        fn get_text(&self, _url: &str) -> Result<String, UpdaterError> {
            Ok(self.text.clone())
        }

        fn download_to_file(
            &self,
            _url: &str,
            dest: &std::path::Path,
            progress: &mut ProgressCallback<'_>,
        ) -> Result<(), UpdaterError> {
            let total = self.download.len() as u64;
            let reader = std::io::Cursor::new(self.download.clone());
            let mut file = std::fs::File::create(dest)?;
            stream_with_progress(reader, &mut file, total, progress)
        }
    }

    #[test]
    fn percent_clamps_and_handles_zero_total() {
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(50, 100), Some(50));
        assert_eq!(percent(100, 100), Some(100));
        assert_eq!(percent(200, 100), Some(100));
    }
}
