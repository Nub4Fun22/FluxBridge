# fluxbridge-desktop

The FluxBridge desktop client: the Slint GUI plus the starter boilerplate for
the device connection, the stream decode loop, and the in-app updater.

## Modules

| Module            | Responsibility                                                                 |
| ----------------- | ------------------------------------------------------------------------------ |
| `connection.rs`   | TCP client, `Handshake` over `PacketCodec`, decoded-packet channel, reconnect + mDNS hooks |
| `decode_loop.rs`  | Pulls `VideoFrame` packets, drives a `VideoDecoder`, pushes `DecodedFrame` to a sink. Ships a `StubDecoder` |
| `updater_ui.rs`   | Glue between `fluxbridge-updater` and the dialog: check on startup, download with progress, apply |
| `ui.rs`           | Slint integration (behind the `gui` feature): wires the `UpdateDialog` callbacks |
| `main.rs`         | Wires everything; default target `127.0.0.1:27183`, runs the event loop        |

## The `gui` feature (Slint)

Slint is a hardware-accelerated GUI toolkit whose backends need a display server
and system GUI libraries. On the headless CI host those are not guaranteed, so
**Slint is gated behind the `gui` cargo feature, which is OFF by default.**

* `cargo build` / `cargo build --workspace` (default, no `gui`) compiles all the
  pure-Rust logic (connection, decode loop, updater glue) and a small headless
  `main` that runs the startup update check and attempts the pipeline. No
  display or GUI libraries are required. This is what CI verifies.
* `cargo build -p fluxbridge-desktop --features gui` compiles `ui/app.slint`
  (via `build.rs` + `slint-build`) and the real Slint window. The configured
  Slint backend is `backend-winit` with the software renderer
  (`renderer-software`), chosen so compilation does not depend on a GPU or
  vendor GL stack. Run this on a developer workstation with a display.

All GUI-independent logic lives outside `ui.rs` and is unit tested, so the two
update buttons' behaviour (`Actualizeaza acum` / `Mai tarziu / Refuza`), the
download-with-progress flow, and the decode loop are all covered by
`cargo test` regardless of the feature.

## Build and test

```sh
export CARGO_HOME=/tmp/cargo-home   # default ~/.cargo is read-only in the sandbox
cargo build --workspace             # headless, default features
cargo test  --workspace             # updater + decode-loop + connection tests
cargo build -p fluxbridge-desktop --features gui   # compile the Slint UI
```

## Run

```sh
# Headless scaffold (no GUI): prints the update-check result and pipeline state.
cargo run -p fluxbridge-desktop -- 127.0.0.1:27183

# With the GUI window (needs a display):
cargo run -p fluxbridge-desktop --features gui -- 192.168.1.42:27183
```

The optional positional argument is the `host:port` of the Android capture
server; it defaults to `127.0.0.1:27183` (the adb-forward address).
