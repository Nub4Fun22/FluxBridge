# FluxBridge roadmap

The project is delivered in four phases. Each phase leaves the workspace in a
buildable, tested state and layers new capability on the decoupled core.

## Delivered in this pass (Android v1 functional milestone)

What is IMPLEMENTED and unit-tested headless in the current build (the headless
default build and tests are green and the `gui` build compiles):

- The adb transport layer over USB and Wi-Fi: adb discovery, the exact command
  lines to push and launch the scrcpy server, and the local port forward
  (`crates/desktop/src/adb.rs`), all covered by exact-argument unit tests.
- The scrcpy wire format: the one-shot video header, the tunnel-forward dummy
  byte, the length-prefixed H.264 access-unit framing, and the control-message
  encoders for touch and key injection (`crates/desktop/src/scrcpy.rs`), covered
  by synthetic-stream and exact-byte unit tests.
- The mirroring pipeline (`crates/desktop/src/pipeline.rs`): locate adb, push
  and launch the server, set up the forward, connect the video socket, drive the
  scrcpy stream into the decoder, open the scrcpy control socket and build the
  input sink from the device resolution, and tear everything down. The
  scrcpy-driven video path and the control-sink handoff are exercised by
  headless tests against in-memory streams and fake decoders / writers.
- The decoder seam: a real FFmpeg H.264 decoder behind the OFF-by-default
  `decode` feature (FFmpeg via `ffmpeg-next`), with a pure-Rust `StubDecoder`
  keeping the default and `gui` builds link-free. Decoded RGBA frames are drawn
  into the Slint video surface.
- Updater discoverability: the automatic startup check is joined by a manual
  "Check for updates" entry in the Start menu, with visible up-to-date,
  update-available and failure feedback.
- Application icon: an original cast/screen-share glyph authored as
  `assets/icon.svg`, generated into a square multi-resolution `assets/icon.ico`,
  set as the live Slint window icon, embedded into the Windows `.exe`, and wired
  into the installer's Setup icon and shortcuts.
- UX fixes: no stray console window on the Windows GUI release build, a
  scrollable Help panel, and a landing Start menu (Start / Connect wirelessly /
  Check for updates / Help / Quit).
- Zero-setup adb: when adb is not installed, FluxBridge downloads Google's
  official platform-tools into a per-user data dir on first use and uses the
  bundled copy directly (no PATH edits, no admin rights), with a graceful
  offline fallback to the manual install guidance
  (`crates/desktop/src/provision.rs`). The download/extract/path logic is pure
  and unit-tested with a fake transport and a synthetic in-memory zip.
- mDNS wireless auto-pairing (Android 11+ Wireless debugging): discover devices
  by driving `adb mdns services` and parsing its output, pair a new device once
  with the 6-digit code the phone shows (`adb pair`), `adb connect` on the
  dynamic mDNS endpoint, and remember the device so later launches auto-reconnect
  silently (`crates/desktop/src/adb.rs`, `devices.rs`, `wireless_ui.rs`, and the
  Slint "Connect wirelessly" panel). The mDNS parsing, endpoint selection, code
  validation, and device persistence are pure and unit-tested; the live
  discovery/pairing/mirroring is verified by the user on Windows with a phone.
  The one-time 6-digit code is required by Android and is read off the phone on
  the first pairing only.

What is WIRED but not yet verified end to end: live device streaming and input
injection. The transport, scrcpy framing, control encoding, and decode seam are
all connected into the pipeline, but a real Android phone over a real scrcpy
server cannot be exercised on the headless Linux CI host (no device, no adb, no
FFmpeg dev libraries, no display). End-to-end mirroring must be verified by the
user on Windows with a phone attached: build `--features gui,decode`, bundle the
scrcpy server jar next to the `.exe` (the installer does this), connect a device,
and click Start. Until that pass, treat live mirroring and input as implemented
and wired but not yet field-verified.

Still deferred (tracked in the phases below, with Bluetooth video explicitly
NOT viable): bi-directional drag-and-drop, semantic clipboard sync, the floating
tray "pill", the virtual secondary display, the developer side panel, and iOS.
Bluetooth is not a video transport: its
bandwidth is roughly one to two orders of magnitude too low for live H.264, so
it could only ever be a future low-bandwidth side channel (clipboard,
notifications), never the screen stream.

## Phase 1: Windows MVP + basic Android USB stream and input

- Cargo workspace, `fluxbridge-core` (wire protocol + traits) and the Slint
  desktop shell.
- Android capture server stub (`app_process` + `SurfaceControl` + `MediaCodec`)
  streaming H.264 over an ADB USB socket.
- FFmpeg + DXVA2 / D3D11VA hardware decode backend on Windows.
- Basic bi-directional control: mouse, keyboard and touch events forwarded to
  the device via the `InputSink` backend.
- Connection skeleton and stream decode loop in the desktop crate.

## Phase 2: Bi-directional drag-and-drop, floating tray widget, wireless auto-docking

- Context-aware drag-and-drop both directions using the OLE `DragDropBridge`
  backend on Windows (inject into foreground app; native drop onto Explorer).
- Semantic clipboard sync (text, indented code, images) with searchable history.
- Floating tray "pill" widget: 2FA SMS code extraction, media controls,
  battery/signal, without the full video stream running.
- Global shortcuts (for example Alt+Space to show, Esc to hide). Note: wireless
  auto-pairing via mDNS discovery and silent reconnection is already DELIVERED
  (see the Android v1 functional milestone above), ahead of the original phasing.

## Phase 3: Auto-update module, iOS capture and actions

- `fluxbridge-updater` crate: startup check against the GitHub Releases API,
  non-intrusive "update now / later" dialog with changelog, background download
  with progress, apply and restart.
- iOS integration following [`IOS_STRATEGY.md`](IOS_STRATEGY.md): AirPlay
  mirroring / RTP H.264 capture and WebDriverAgent-based control, within Apple
  ecosystem limits.
- Developer side panel: filtered Logcat, UI inspector, network monitor.

## Phase 4: Linux port and optimization (Wayland / PipeWire)

- Linux backends for the OS-facing traits: PipeWire capture, VA-API / VDPAU
  decode, XDnD (X11) and `wl_data_device` (Wayland) drag-and-drop, clipboard.
- Wayland and X11 windowing parity with the Windows host.
- Performance tuning: zero-copy frame paths, latency and CPU/GPU profiling,
  release builds with LTO and symbol stripping.
