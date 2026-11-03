//! Build script for `scrcpy-decode-sys`.
//!
//! The native engine (the vendored scrcpy C plus the FluxBridge `fb_decoder`
//! wrapper) is compiled and the FFmpeg libraries are linked ONLY when the
//! `build-native` cargo feature is enabled. With the feature OFF this script is
//! a clean no-op: no C is compiled and no library is linked, so the crate (and
//! the whole workspace) builds on the headless Linux host, which has no FFmpeg.
//!
//! When `build-native` is ON it uses the `cc` crate to compile
//! `native/scrcpy-decode/fb_decoder.c` and `native/scrcpy-decode/vendor/
//! packet_merger.c`, adds the FFmpeg include dir from the `FFMPEG_DIR`
//! environment variable (the same env the release workflow sets), and emits the
//! cargo link directives for avcodec / avformat / avutil / swscale against the
//! `FFMPEG_DIR/lib` search path. The host has cc/gcc/clang so `cc` loads, but
//! FFmpeg is absent, so the full native link is confirmed only on windows-latest
//! CI; here the only thing verified is that build.rs itself compiles and that
//! the feature-OFF path is a no-op.

fn main() {
    // Rerun if the feature flag or the vendored sources change.
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(feature = "build-native")]
    build_native();

    #[cfg(not(feature = "build-native"))]
    {
        // Feature OFF: a deliberate no-op. The crate exposes only the FFI
        // declarations in src/lib.rs; nothing links FFmpeg, which is why the
        // default workspace build stays green on the headless Linux host.
    }
}

#[cfg(feature = "build-native")]
fn build_native() {
    use std::path::PathBuf;

    // The vendored C lives under <workspace>/native/scrcpy-decode. This crate
    // is at <workspace>/crates/scrcpy-decode-sys, so go up two levels.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let native_dir = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|ws| ws.join("native").join("scrcpy-decode"))
        .expect("resolve <workspace>/native/scrcpy-decode");

    let fb_decoder_c = native_dir.join("fb_decoder.c");
    let packet_merger_c = native_dir.join("vendor").join("packet_merger.c");

    println!("cargo:rerun-if-changed={}", fb_decoder_c.display());
    println!("cargo:rerun-if-changed={}", packet_merger_c.display());
    println!(
        "cargo:rerun-if-changed={}",
        native_dir.join("fb_decoder.h").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        native_dir.join("vendor").join("packet_merger.h").display()
    );
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");

    // FFMPEG_DIR points at the root of the provisioned FFmpeg (BtbN shared
    // build on CI). Its `include` dir carries the libav* headers and its `lib`
    // dir the import libraries to link against.
    let ffmpeg_dir = std::env::var("FFMPEG_DIR").expect(
        "FFMPEG_DIR must be set when building scrcpy-decode-sys with \
         --features build-native (the release workflow provisions FFmpeg and \
         sets it); it is unset and unused on the headless host where the \
         feature is OFF",
    );
    let ffmpeg_dir = PathBuf::from(ffmpeg_dir);
    let ffmpeg_include = ffmpeg_dir.join("include");
    let ffmpeg_lib = ffmpeg_dir.join("lib");

    let mut build = cc::Build::new();
    build
        .file(&fb_decoder_c)
        .file(&packet_merger_c)
        .include(&native_dir)
        .include(&ffmpeg_include);
    build.compile("fb_decoder");

    // Link the FFmpeg libraries the engine uses. Order after the static
    // fb_decoder archive (emitted by cc above) so the linker resolves the
    // engine's references into them.
    println!("cargo:rustc-link-search=native={}", ffmpeg_lib.display());
    for lib in ["avcodec", "avformat", "avutil", "swscale"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
