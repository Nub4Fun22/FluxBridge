//! Build script for fluxbridge-desktop.
//!
//! When the `gui` feature is enabled, this compiles the Slint markup in
//! `ui/app.slint` into generated Rust that `src/ui.rs` includes. Without the
//! `gui` feature (the default, used on the headless CI host), the build script
//! is a no-op so the crate compiles without a display server or Slint's native
//! GUI dependencies.

fn main() {
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
