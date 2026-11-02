# FluxBridge roadmap

The project is delivered in four phases. Each phase leaves the workspace in a
buildable, tested state and layers new capability on the decoupled core.

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
- Wireless auto-pairing via mDNS discovery and reconnection; global shortcuts
  (for example Alt+Space to show, Esc to hide).

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
