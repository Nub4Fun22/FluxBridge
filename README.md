# FluxBridge

A modern, ultra-performant desktop application for **mirroring, remote control
and advanced synchronization** of smartphones on a PC. Windows-first, with a
modular architecture engineered for a clean port to Linux (Wayland and X11).

FluxBridge is built in **Rust** with a hardware-accelerated **Slint** UI. The
core is strictly decoupled from operating-system and vendor APIs: every
platform capability (FFmpeg hardware decode, ADB, Win32 OLE, Wayland/X11,
libimobiledevice) sits behind a trait with a compile-ready stub, so the whole
workspace builds and tests on any host, including headless CI.

## Repository name

Several short, professional names combining the ideas of *flux*, *bridge*,
*mirror* and *link* were considered:

| Candidate   | Idea |
|-------------|------|
| **FluxBridge** | Flowing media/data (flux) across a bridge between phone and PC. |
| MirrorLink  | Mirroring plus a device link (collides with the automotive MirrorLink standard). |
| PhoneFlux   | Phone-centric flux of frames and events. |
| Linkspan    | A span/link between devices. |
| Reflo       | Reflect + flow, very short but abstract. |

**Chosen name: `FluxBridge`.**

Justification: it captures both halves of the product in one word. *Flux*
communicates the continuous, low-latency flow of video frames, input events and
synchronized clipboard/file data; *Bridge* communicates the bi-directional link
that connects two different environments (the phone OS and the desktop OS). It
is short, pronounceable, unambiguous in English, has no collision with an
existing connectivity standard (unlike MirrorLink), and reads well as a crate
prefix (`fluxbridge-core`, `fluxbridge-desktop`, `fluxbridge-updater`) and as a
Windows product/installer name.

## Target platforms

**Desktop host**

- Windows 10 / 11 (primary initial target: Win32, Direct3D, OLE drag-and-drop).
- Linux (planned port): Wayland and X11, PipeWire capture, VA-API / VDPAU decode.

**Connected mobile devices**

- **Android:** full bi-directional control, minimal-latency H.264/H.265 video via
  hardware acceleration, connection over ADB (USB and Wi-Fi).
- **iOS / iPhone:** realistic integration within Apple's ecosystem limits; the
  trade-offs between an internal AirPlay mirroring server / mDNS / RTP H.264
  stream and libimobiledevice / WebDriverAgent control are analyzed in
  [`docs/IOS_STRATEGY.md`](docs/IOS_STRATEGY.md).

## Feature overview

- Low-latency screen mirroring with hardware-accelerated decode.
- Bi-directional remote control (mouse, keyboard, multi-touch).
- Complete bi-directional, context-aware drag-and-drop (PC to phone and phone to PC).
- Semantic clipboard sync (text, indented code, images) with a searchable history.
- Floating tray "pill" widget (2FA code extraction, media controls, battery/signal).
- Virtual secondary display / app sandboxing on Android via `DisplayManager`.
- Toggleable developer side panel (filtered Logcat, UI inspector, network monitor).
- Wireless auto-pairing via mDNS and global keyboard shortcuts.
- Intelligent in-app updater backed by the GitHub Releases API.

## Repository layout

```
fluxbridge/
  Cargo.toml                 Cargo workspace manifest
  Cargo.lock                 Committed (the workspace builds binaries)
  README.md                  This file
  CONTRIBUTING.md            Branching strategy, SemVer, commit conventions
  LICENSE                    MIT
  .gitignore
  crates/
    core/                    fluxbridge-core: wire protocol + decoupling traits
      Cargo.toml
      src/
        lib.rs
        error.rs             ProtocolError / CoreError
        protocol.rs          Binary packet codec + typed payloads + tests
        traits.rs            VideoCapture, VideoDecoder, InputSink, ...
        platform/
          mod.rs             #[cfg]-gated platform selection
          windows.rs         Win32 OLE / FFmpeg DXVA2 stubs (#[cfg(windows)])
          linux.rs           XDnD/Wayland / FFmpeg VA-API stubs (#[cfg(unix)])
    updater/                 fluxbridge-updater: GitHub Releases updater
    desktop/                 fluxbridge-desktop: Slint GUI binary (gui feature)
  android-server/            Kotlin app_process capture server stub
    build.gradle.kts         Minimal placeholder build
    README.md                Build + adb (USB/Wi-Fi) + protocol flow
    src/main/java/com/fluxbridge/server/
      Server.kt              SurfaceControl/MediaCodec/InputManager/DisplayManager stubs
  installer/
    fluxbridge.iss           Inno Setup script (fully commented)
  .github/workflows/
    release.yml              Tag-triggered CI: build + .zip + Setup.exe + Release
  docs/
    ARCHITECTURE.md          Module blueprint + diagram
    PROTOCOL.md              Exact binary packet format
    ROADMAP.md               4-phase implementation plan
    IOS_STRATEGY.md          iOS capture/control strategy analysis
```

> Note: the workspace manifest lists only Rust crate members. `android-server/`
> (Kotlin, built with the Android toolchain), `installer/fluxbridge.iss` (Inno
> Setup, compiled on Windows) and `.github/workflows/release.yml` are not part
> of the Rust build, so `cargo build --workspace` is unaffected by them.

## Building

Rust (1.75+; developed against rustc/cargo 1.92) is required.

```sh
cd fluxbridge
cargo build --workspace
cargo test  --workspace
```

The desktop GUI is behind an **OFF-by-default `gui` cargo feature** so the
default/headless build (and CI) stays free of a display server and GUI system
libraries. Build the actual windowed client with:

```sh
cargo build --release --features gui --bin fluxbridge-desktop
```

### CARGO_HOME note for the sandbox / CI

In this development sandbox the default `CARGO_HOME` (`~/.cargo`) is on a
read-only filesystem. Export a writable one before any cargo command, otherwise
cargo fails with `Read-only file system (os error 30)`:

```sh
mkdir -p /tmp/cargo-home
export CARGO_HOME=/tmp/cargo-home

cargo build --workspace
cargo test  --workspace
cargo clippy --workspace
```

## Releases and auto-update

Releases are fully automated by [`.github/workflows/release.yml`](.github/workflows/release.yml).
The end-to-end flow is:

1. **Tag a version.** Push a SemVer tag, for example:

   ```sh
   git tag v1.2.0
   git push origin v1.2.0
   ```

   The `v*` / `v*.*.*` tag triggers the `release` workflow on a `windows-latest`
   runner.
2. **Optimized build.** CI runs `cargo build --release --features gui`. The
   optimization the user asked for (LTO + stripped debug symbols) lives in
   `[profile.release]` in the workspace `Cargo.toml`
   (`lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`), so
   the workflow is a thin wrapper and the result is reproducible.
3. **Package two artifacts:** a **portable `.zip`**
   (`FluxBridge-<tag>-portable.zip`) of the binary, and a **Setup installer**
   `.exe` produced by compiling [`installer/fluxbridge.iss`](installer/fluxbridge.iss)
   with Inno Setup (installed via Chocolatey). The installer offers an install
   path + license-acceptance wizard, a **Desktop shortcut enabled by default**,
   a Start Menu entry, and automatic Windows uninstall registration.
4. **Publish a GitHub Release.** `softprops/action-gh-release` creates the
   release for the tag and attaches both the `.zip` and the Setup `.exe`.
5. **In-app update.** On startup the desktop client's updater
   (`crates/updater`) queries the GitHub Releases API, compares the latest
   `tag_name` against its own version with SemVer, and if newer surfaces a
   non-intrusive notification. It downloads the release asset (it **prefers the
   `.exe`**, i.e. the Setup installer this workflow uploads) with a progress
   bar, then launches it to apply the update. The installer's `.exe` naming and
   the updater's asset selection are intentionally consistent.

The Android capture server is built and shipped separately (see
[`android-server/README.md`](android-server/README.md)); it is launched on the
device over `adb` and is not part of the desktop release.

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) - module blueprint and how the
  core stays decoupled from OS APIs.
- [`docs/PROTOCOL.md`](docs/PROTOCOL.md) - exact binary wire-protocol format.
- [`docs/ROADMAP.md`](docs/ROADMAP.md) - the four implementation phases.
- [`docs/IOS_STRATEGY.md`](docs/IOS_STRATEGY.md) - iOS capture and control strategy.

## License

Licensed under the [MIT License](LICENSE).
