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
}
