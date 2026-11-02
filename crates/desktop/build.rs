//! Build script for fluxbridge-desktop.
//!
//! When the `gui` feature is enabled, this compiles the Slint markup in
//! `ui/app.slint` into generated Rust that `src/ui.rs` includes. Without the
//! `gui` feature (the default, used on the headless CI host), the build script
//! is a no-op so the crate compiles without a display server or Slint's native
//! GUI dependencies.

fn main() {
    // Authoritatively stamp the shipped binary's self-reported version from the
    // release tag. The release workflow (.github/workflows/release.yml) derives
    // the version from the pushed git tag (github.ref_name, with any leading
    // `v` stripped) and exports it as FLUXBRIDGE_RELEASE_VERSION before
    // `cargo build --release`. When that env var is present we re-emit it as
    // FLUXBRIDGE_BUILD_VERSION, which `src/main.rs` reads via `option_env!` in
    // preference to CARGO_PKG_VERSION.
    //
    // This guarantees the binary built at tag vX.Y.Z reports exactly X.Y.Z even
    // if someone forgot to bump the workspace version in Cargo.toml, so the
    // updater can never re-offer a version the user already installed (the
    // infinite-update-loop bug). When the env var is absent (every normal
    // `cargo build`/`cargo test` on the headless host and developer machines)
    // nothing is emitted and main.rs falls back to CARGO_PKG_VERSION, so the
    // default build is unaffected.
    println!("cargo:rerun-if-env-changed=FLUXBRIDGE_RELEASE_VERSION");
    if let Ok(release_version) = std::env::var("FLUXBRIDGE_RELEASE_VERSION") {
        let trimmed = release_version.trim();
        if !trimmed.is_empty() {
            let stamped = trimmed.strip_prefix('v').unwrap_or(trimmed);
            println!("cargo:rustc-env=FLUXBRIDGE_BUILD_VERSION={stamped}");
        }
    }

    #[cfg(feature = "gui")]
    {
        slint_build::compile("ui/app.slint").expect("failed to compile ui/app.slint");
    }
    println!("cargo:rerun-if-changed=ui/app.slint");

    // Embed the application icon into the Windows .exe so Explorer, the taskbar
    // and the Alt+Tab switcher show it. This is Windows-only: `embed-resource`
    // is declared under [target.'cfg(windows)'.build-dependencies], so on Linux
    // the crate is not even compiled and this block is a no-op (the whole
    // function compiles and the headless/gui Linux builds are unaffected).
    //
    // `embed-resource` compiles the small fluxbridge.rc next to this build
    // script; that .rc points at ..\..\assets\icon.ico relative to itself
    // (crates/desktop -> repo root). The rerun-if-changed lines below use the
    // same crate-relative paths so edits to either the icon or the .rc retrigger
    // the build.
    #[cfg(windows)]
    {
        embed_resource::compile("fluxbridge.rc", embed_resource::NONE);
    }
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    println!("cargo:rerun-if-changed=fluxbridge.rc");
}
