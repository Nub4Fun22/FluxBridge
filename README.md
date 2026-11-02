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
- Wireless discovery and one-time-code pairing via mDNS (Android 11+ Wireless
  debugging), with silent auto-reconnect to a remembered device on later
  launches. Global keyboard shortcuts are planned.
- Intelligent in-app updater backed by the GitHub Releases API. FluxBridge
  checks for updates automatically at startup, and the Start menu's "Check for
  updates" button re-runs the check on demand (it reports when you are already
  on the latest version, offers any newer release, or shows an error).

## How the connection works (Android v1)

FluxBridge v1 mirrors and controls Android over **ADB**, supporting two
transports:

- **USB:** plug the phone in, `adb forward tcp:<port> localabstract:scrcpy`
  tunnels a local socket to the device.
- **Wi-Fi (automatic discovery + one-time pairing):** FluxBridge uses the
  Android 11+ Wireless debugging flow, so you never type an IP by hand. Enable
  Wireless debugging on the phone (same Wi-Fi as the PC), open "Connect
  wirelessly" and click Scan. FluxBridge discovers the phone over mDNS by
  driving `adb mdns services`. The first time, tap "Pair device with pairing
  code" on the phone and type the shown 6-digit code; FluxBridge runs `adb pair`
  then `adb connect` and remembers the device, so later launches reconnect
  automatically. The one-time 6-digit code is shown only on the phone, so that
  very first pairing is not zero-touch; everything after it is. adb itself is
  auto-provisioned: if it is not installed, FluxBridge downloads Google's
  official platform-tools on first use and keeps a private copy (no PATH edits,
  no admin rights), falling back to the manual "install adb" guidance only when
  that download is blocked offline.

The device-side server is the upstream, MIT-licensed **scrcpy server** jar,
pushed to `/data/local/tmp` and launched with `app_process` (no app install).
FluxBridge targets scrcpy server version `2.4`; the jar is bundled on the
Windows side and is not vendored into this repo. See
[`android-server/README.md`](android-server/README.md) for the full rationale.

Two wire formats are in play, and they are kept distinct in code:

- **scrcpy sockets** (the shipped v1 video + control path) use scrcpy's own
  framing: a one-shot video header, length-prefixed H.264 access units, and
  scrcpy control messages. Parsing/encoding lives in
  `crates/desktop/src/scrcpy.rs`; the adb command construction lives in
  `crates/desktop/src/adb.rs` (pure functions, unit-tested).
- **FluxBridge `0xFB` protocol** (`crates/core/src/protocol.rs`) is retained for
  a future FluxBridge-native server and is **not** used on scrcpy sockets.

The pipeline connects the scrcpy video socket, drives the stream into the
decoder, and opens the scrcpy control socket for input injection
(`crates/desktop/src/pipeline.rs`). The installer ships `scrcpy-server.jar` next
to `fluxbridge-desktop.exe`, and the app resolves it relative to the executable
directory, so an installed build finds the jar regardless of its working
directory.

> **Verification status.** The transport, scrcpy framing, control encoding, and
> decode seam are implemented and unit-tested headlessly (synthetic streams, a
> fake decoder, and a fake control writer). Live end-to-end mirroring and input
> on a real device are wired but must be verified on Windows with a phone
> attached (`--features gui,decode`): the headless CI host has no device, adb,
> FFmpeg dev libraries, or display, so it cannot exercise the live path.

> **No Bluetooth video.** Bluetooth bandwidth is roughly one to two orders of
> magnitude too low to carry a live H.264 screen stream, so FluxBridge does not
> and will not mirror over Bluetooth. USB and Wi-Fi are the supported transports;
> Bluetooth could only ever serve as a future low-bandwidth side channel.

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
  assets/
    icon.svg                 Hand-authored cast/screen-share glyph (source)
    icon.ico                 Generated square multi-size Windows icon (16/32/48/256)
    icon-256.png             Generated 256px PNG embedded as the Slint window icon
  xtask/                     Dev-only icon generator (excluded from the workspace)
    Cargo.toml
    src/main.rs              `cargo run --manifest-path xtask/Cargo.toml -- icon`
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

### Cargo feature matrix

The desktop crate exposes a small set of opt-in features so the same source
tree builds on a headless Linux CI host and on a Windows developer workstation
without changing any code:

| Feature   | Default | What it enables | Where it links |
|-----------|---------|-----------------|----------------|
| (none)    | yes     | Headless logic only: connection skeleton, decode loop driven by the pure-Rust `StubDecoder`, and updater wiring. Zero native/GUI/FFmpeg linkage. | Any host, including headless Linux CI. |
| `gui`     | no      | Compiles and wires the Slint UI (winit backend, software renderer). Does **not** pull FFmpeg. | Links on both Linux and Windows. |
| `decode`  | no      | Real FFmpeg H.264 decode via the `ffmpeg-next` crate, backing the real `VideoDecoder`. | Compiled and verified on **Windows only**. On headless Linux it may fail to link without the libav development packages, which is expected; leave it off there. |

Features compose, so a full Windows client build is:

```sh
cargo build --release --features "gui decode" --bin fluxbridge-desktop
```

`gui` deliberately does not imply `decode`: FFmpeg cannot link on the headless
Linux host, so keeping `decode` out of the default and `gui` feature sets lets
`cargo build --workspace` and `cargo build --features gui` stay green there. The
`.exe` icon resource is embedded through a Windows-only build dependency gated
behind `[target.'cfg(windows)'.build-dependencies]`, so it never affects the
Linux build either.

### Regenerating the application icon

The app icon is an original rendering of the generic "cast / screen-share"
glyph: a rounded-corner display outline, open at the lower-left corner, with
three concentric Wi-Fi-style quarter arcs radiating from that corner. It is
authored by hand as vector primitives in
[`assets/icon.svg`](assets/icon.svg) (no traced or embedded third-party asset).

The committed [`assets/icon.ico`](assets/icon.ico) is a square,
multi-resolution Windows icon (16, 32, 48 and 256 px) generated from the SVG,
along with `assets/icon-256.png` which the GUI embeds for the live window icon.
Generation is a developer-only [`xtask`](xtask) that is deliberately excluded
from the Cargo workspace, so its pure-Rust rendering crates
(`resvg` / `usvg` / `tiny-skia` / `ico` / `png`) never enter
`cargo build --workspace`, the `gui` build, or the shipped binary. Regenerate
both artifacts after editing the SVG with:

```sh
cargo run --manifest-path xtask/Cargo.toml -- icon
```

The icon reaches the user three ways: the Slint window sets it at startup from
the embedded PNG (`gui` build); `crates/desktop/build.rs` embeds `icon.ico` into
the Windows `.exe` via the Windows-only `embed-resource` build dependency (a
no-op on Linux); and the Inno Setup installer sets it as the Setup icon and on
the Start Menu and Desktop shortcuts.

### Windows FFmpeg (decode feature) requirement

The `decode` feature links the real FFmpeg H.264 decoder through `ffmpeg-next`,
which needs the libav development libraries at build time. On Windows the
supported path is [vcpkg](https://github.com/microsoft/vcpkg):

```sh
vcpkg install ffmpeg:x64-windows
```

then point `ffmpeg-next` at it before building, for example by exporting
`FFMPEG_DIR` (and `PKG_CONFIG_PATH`) to the vcpkg install tree, and build with
`--features "gui decode"`. On the headless Linux build (and in the current CI
release job) `decode` is left off, so no libav libraries are required there.

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
