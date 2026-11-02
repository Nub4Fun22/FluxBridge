# FluxBridge Android capture server

> ## v1 decision: FluxBridge drives the upstream scrcpy server
>
> **For v1, FluxBridge does NOT ship the Kotlin server in this directory.**
> Instead it drives the upstream, MIT-licensed
> [`scrcpy-server`](https://github.com/Genymobile/scrcpy) jar: the jar is pushed
> to `/data/local/tmp/scrcpy-server.jar` and launched with `app_process` under
> the shell UID, exactly as scrcpy itself does. This gives FluxBridge a mature,
> battle-tested H.264 capture + input-injection server on day one instead of a
> half-finished bespoke one.
>
> * **Targeted scrcpy server version:** `5.0.1` (see `SCRCPY_SERVER_VERSION` in
>   `crates/desktop/src/adb.rs`). The desktop client passes this version string
>   to `com.genymobile.scrcpy.Server`; the pushed jar must match.
> * **Where the jar comes from:** it is obtained and bundled on the **Windows
>   packaging side** (the Inno Setup installer ships it next to the .exe). The
>   `.jar` binary is deliberately **NOT vendored into this repository** and is
>   not part of the Rust workspace build. License compliance: scrcpy is MIT; its
>   license is reproduced alongside the bundled jar in the Windows package.
> * **Sockets are scrcpy framing, not FluxBridge 0xFB.** The scrcpy video and
>   control sockets (exposed on `localabstract:scrcpy`) use scrcpy's own wire
>   format, parsed/encoded in `crates/desktop/src/scrcpy.rs`. The FluxBridge
>   `0xFB` [`PacketCodec`] protocol in `crates/core/src/protocol.rs` is retained
>   for a future FluxBridge-native server but is **not** used on scrcpy sockets.
>
> ## This Kotlin skeleton is reference-only
>
> The `Server.kt` below is **retained purely as documentation** of the
> equivalent `app_process` capture/inject technique (and of the FluxBridge-native
> 0xFB protocol). **It is not the shipped v1 server** and is not compiled by the
> Rust workspace or CI. If a FluxBridge-native server is ever built out, this is
> the starting point; until then, scrcpy is the server.

The rest of this document describes the device-side half of the FluxBridge link
(whether scrcpy or, eventually, this skeleton); the PC-side half lives in
`crates/desktop`.

> Status of this skeleton: the protocol framing and the server accept/dispatch
> loop are real; the framework calls (SurfaceControl, MediaCodec, InputManager,
> DisplayManager) are documented stubs. This module is intentionally **not**
> compiled by the Rust workspace or the CI sandbox (there is no Android SDK
> there). It would be built out of band with the Android toolchain.

## Why `app_process` and not an APK

The server is launched with `app_process` (the same loader that starts every
Android app), running under the **shell UID** rather than installed as a
sandboxed app. That elevated UID is what lets it reach the hidden/`@hide`
framework APIs it needs (`SurfaceControl`, `InputManager`, `DisplayManager`),
which an ordinary installed APK cannot call. This is the technique scrcpy uses.

So there is no `AndroidManifest.xml`, no `Activity`, and no installation step on
the device: the compiled classes are pushed to `/data/local/tmp` and executed
directly.

## Layout

```
android-server/
  build.gradle.kts                         Minimal placeholder build (see below)
  src/main/java/com/fluxbridge/server/
    Server.kt                              Entry point + protocol codec + stubs:
                                             ScreenEncoder (SurfaceControl+MediaCodec)
                                             InputInjector (InputManager)
                                             FileTransferServer (dedicated socket)
                                             VirtualDisplayManager (DisplayManager)
```

## Building

The server compiles to a plain JVM `.jar`, which is then dex'd so it runs under
ART on the device. `build.gradle.kts` is a placeholder; a real build:

```sh
# 1. Compile Kotlin -> JVM classes -> a fat jar.
cd android-server
./gradlew build            # produces build/libs/android-server-0.1.0.jar

# 2. Dex the jar so ART can load it (d8 ships with the Android build-tools).
d8 --output build/dex build/libs/android-server-0.1.0.jar
#    build/dex/classes.dex  ->  repackage into fluxbridge-server.jar
```

The hidden framework classes are put on the **compile-only** classpath via a
stub `android.jar` (see the commented `compileOnly` line in `build.gradle.kts`)
so nothing from the SDK is bundled into the shipped jar.

## Connecting from the desktop client (adb flow)

The desktop client defaults to `127.0.0.1:27183`. adb forwards that local port
to the server running on the device.

### USB

```sh
# Push the dex'd server jar to the device.
adb push fluxbridge-server.jar /data/local/tmp/

# Forward the desktop's local port to the device-side server port.
adb forward tcp:27183 tcp:27183

# Launch the server via app_process (shell UID).
adb shell CLASSPATH=/data/local/tmp/fluxbridge-server.jar \
    app_process / com.fluxbridge.server.Server 27183
```

Then start `fluxbridge-desktop`; its connection skeleton dials
`127.0.0.1:27183`, which adb tunnels to the device.

### Wi-Fi

```sh
# One-time (with the device on USB): enable TCP/IP mode, then read its IP.
adb tcpip 5555
adb shell ip route            # note the device's wlan0 address, e.g. 192.168.1.42

# Reconnect over the network and repeat the push + forward + launch above.
adb connect 192.168.1.42:5555
adb -s 192.168.1.42:5555 forward tcp:27183 tcp:27183
adb -s 192.168.1.42:5555 shell CLASSPATH=/data/local/tmp/fluxbridge-server.jar \
    app_process / com.fluxbridge.server.Server 27183
```

Phase 2 of the roadmap replaces the manual `adb connect` step with mDNS
auto-pairing; the desktop client already has a `discover_via_mdns` hook.

## Protocol

The server speaks the exact binary protocol specified in
[`../docs/PROTOCOL.md`](../docs/PROTOCOL.md): an 8-byte header
(`0xFB 0x01`, `u8` version `1`, `u8` packet type, big-endian `u32` payload
length) followed by the payload. The `Protocol` object and `PacketCodec` class
in `Server.kt` are kept byte-for-byte in sync with
`crates/core/src/protocol.rs`, so the Rust `PacketCodec` on the desktop side and
this Kotlin codec interoperate directly.

Flow:

1. Client connects and sends `HANDSHAKE` (capability exchange); the server
   replies with its own `HANDSHAKE`.
2. The server emits one `VideoConfig` (codec + SPS/PPS/VPS), then a continuous
   stream of `VideoFrame` packets produced by `MediaCodec`.
3. The client sends `MouseEvent` / `KeyEvent` / `TouchEvent`; the server injects
   them with `InputManager`.
4. Large files use a dedicated socket (`FileMeta` + `FileChunk`) so they never
   stall the video stream.
5. `Heartbeat` keep-alives are answered with `Ack`.
