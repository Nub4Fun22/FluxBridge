//! Serde models for the subset of the GitHub Releases REST API we consume.
//!
//! Endpoint: `GET https://api.github.com/repos/{owner}/{repo}/releases/latest`.
//! See <https://docs.github.com/en/rest/releases/releases#get-the-latest-release>.
//! Only the fields the updater needs are modeled; unknown fields are ignored.

use serde::Deserialize;

/// A single downloadable asset attached to a GitHub release.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GithubAsset {
    /// The asset file name, for example `FluxBridge-Setup-1.2.0.exe`.
    pub name: String,
    /// The direct download URL for the asset binary.
    pub browser_download_url: String,
}

/// The `releases/latest` payload (subset).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GithubRelease {
    /// The git tag the release points at, for example `v1.2.0`.
    pub tag_name: String,
    /// The Markdown release notes / changelog body.
    #[serde(default)]
    pub body: String,
    /// Whether this is a draft (unpublished) release.
    #[serde(default)]
    pub draft: bool,
    /// Whether GitHub flagged this as a pre-release.
    #[serde(default)]
    pub prerelease: bool,
    /// The assets attached to the release.
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

impl GithubRelease {
    /// Build the canonical `releases/latest` URL for `owner`/`repo`.
    pub fn latest_url(owner: &str, repo: &str) -> String {
        format!("https://api.github.com/repos/{owner}/{repo}/releases/latest")
    }

    /// Pick the asset best matching the host platform.
    ///
    /// On Windows a `.exe` or `.msi` installer is preferred. On other platforms
    /// the first asset is returned (the headless host has no native installer
    /// format, so the stub simply downloads whatever is published). Returns
    /// `None` when the release has no assets at all.
    pub fn select_asset(&self) -> Option<&GithubAsset> {
        if self.assets.is_empty() {
            return None;
        }
        #[cfg(windows)]
        {
            if let Some(asset) = self.assets.iter().find(|a| {
                let lower = a.name.to_ascii_lowercase();
                lower.ends_with(".exe") || lower.ends_with(".msi")
            }) {
                return Some(asset);
            }
        }
        // Non-Windows (or Windows with no installer asset): prefer an `.exe`
        // when present so the documented selection stays deterministic in
        // cross-platform tests, otherwise take the first asset.
        self.assets
            .iter()
            .find(|a| a.name.to_ascii_lowercase().ends_with(".exe"))
            .or_else(|| self.assets.first())
    }
}
