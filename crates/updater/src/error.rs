//! Error type for the updater crate.

use thiserror::Error;

/// Errors produced by the GitHub Releases updater.
#[derive(Debug, Error)]
pub enum UpdaterError {
    /// The HTTP transport failed (network error, non-2xx status, bad body).
    #[error("http transport error: {0}")]
    Http(String),

    /// The GitHub API response could not be parsed as the expected JSON shape.
    #[error("failed to parse release JSON: {0}")]
    Parse(#[from] serde_json::Error),

    /// A version string (current or remote tag) could not be parsed as semver.
    #[error("invalid version string {value:?}: {source}")]
    Version {
        /// The offending version string.
        value: String,
        /// The underlying semver parse error.
        #[source]
        source: semver::Error,
    },

    /// The latest release carried no asset matching the current platform.
    #[error("no downloadable asset found for this platform in release {0}")]
    NoAsset(String),

    /// An I/O error occurred while downloading or applying the update.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result alias for the updater crate.
pub type UpdaterResult<T> = Result<T, UpdaterError>;
