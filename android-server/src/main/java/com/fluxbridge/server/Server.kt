/*
 * FluxBridge Android capture server (SKELETON, REFERENCE-ONLY).
 *
 * NOT THE SHIPPED v1 SERVER. FluxBridge v1 drives the upstream MIT-licensed
 * scrcpy-server jar (pushed to /data/local/tmp and launched via app_process);
 * see android-server/README.md and crates/desktop/src/adb.rs for that decision.
 * This file is retained ONLY as documentation of the equivalent app_process
 * capture/inject technique and of the FluxBridge-native 0xFB protocol. It is a
 * documented, well-formed Kotlin skeleton that is NOT compiled in the FluxBridge
 * CI sandbox (there is no Android SDK there). If it were ever built, it would be
 * built out of band with the Android toolchain and shipped to the device as a
 * plain .jar / .dex launched over adb with app_process:
 *
 *     adb push fluxbridge-server.jar /data/local/tmp/
 *     adb shell CLASSPATH=/data/local/tmp/fluxbridge-server.jar \
 *         app_process / com.fluxbridge.server.Server
 *
 * Running inside app_process (NOT as an installed APK) gives the process the
 * shell UID's elevated permissions, which is what lets it call the hidden
 * SurfaceControl / InputManager / DisplayManager framework APIs that a normal
 * sandboxed app cannot reach. This is the same technique scrcpy uses.
 *
 * Responsibilities of the real server (each stubbed below):
 *   1. Capture the device screen with SurfaceControl and encode it with
 *      MediaCodec as a low-latency H.264 / H.265 elementary stream.
 *   2. Inject remote mouse / keyboard / touch events with InputManager.
 *   3. Serve a dedicated socket for fast, back-pressured file transfer.
 *   4. Create a virtual secondary display via DisplayManager so an app can be
 *      mirrored onto its own off-device surface.
 *
 * Everything is framed with the exact binary protocol documented in
 * docs/PROTOCOL.md (magic 0xFB 0x01, u8 version, u8 type, u32 big-endian
 * length, payload). The desktop client in crates/desktop speaks the same
 * protocol via fluxbridge_core::protocol::PacketCodec.
 */
package com.fluxbridge.server

import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.net.ServerSocket
import java.net.Socket

/**
 * Wire-protocol constants, kept byte-for-byte in sync with
 * `crates/core/src/protocol.rs` and `docs/PROTOCOL.md`.
 */
object Protocol {
    const val MAGIC0: Int = 0xFB
    const val MAGIC1: Int = 0x01
    const val VERSION: Int = 1
    const val HEADER_LEN: Int = 8

    // Packet types (see docs/PROTOCOL.md "Packet types").
    const val HANDSHAKE: Int = 0x01
    const val VIDEO_CONFIG: Int = 0x02
    const val VIDEO_FRAME: Int = 0x03
    const val MOUSE_EVENT: Int = 0x10
    const val KEY_EVENT: Int = 0x11
    const val TOUCH_EVENT: Int = 0x12
    const val CLIPBOARD_TEXT: Int = 0x20
    const val CLIPBOARD_IMAGE: Int = 0x21
    const val FILE_CHUNK: Int = 0x30
    const val FILE_META: Int = 0x31
    const val DRAG_ENTER: Int = 0x40
    const val DRAG_DROP: Int = 0x41
    const val BATTERY: Int = 0x50
    const val ACK: Int = 0x60
    const val HEARTBEAT: Int = 0x61
}

/**
 * A single decoded protocol frame: the one-byte packet type plus its raw
 * payload bytes. Higher layers interpret the payload per docs/PROTOCOL.md.
 */
data class Packet(val type: Int, val payload: ByteArray)

/**
 * Minimal framing reader/writer for the FluxBridge binary protocol.
 *
 * This mirrors `PacketCodec` on the Rust side: an 8-byte fixed header followed
 * by a big-endian u32 length and that many payload bytes. It is deliberately
 * tiny and dependency-free so it can run under app_process.
 */
class PacketCodec(socket: Socket) {
    private val input = DataInputStream(socket.getInputStream().buffered())
    private val output = DataOutputStream(socket.getOutputStream().buffered())

    /** Read exactly one packet, blocking until the full frame has arrived. */
    @Throws(IOException::class)
    fun read(): Packet {
        val m0 = input.readUnsignedByte()
        val m1 = input.readUnsignedByte()
        if (m0 != Protocol.MAGIC0 || m1 != Protocol.MAGIC1) {
            throw IOException("bad magic: $m0 $m1")
        }
        val version = input.readUnsignedByte()
        if (version != Protocol.VERSION) {
            throw IOException("unsupported protocol version: $version")
        }
        val type = input.readUnsignedByte()
        val len = input.readInt() // DataInputStream reads big-endian u32.
        if (len < 0) throw IOException("payload length overflow")
        val payload = ByteArray(len)
        input.readFully(payload)
        return Packet(type, payload)
    }

    /** Frame and write one packet, then flush. */
    @Throws(IOException::class)
    fun write(type: Int, payload: ByteArray) {
        output.writeByte(Protocol.MAGIC0)
        output.writeByte(Protocol.MAGIC1)
        output.writeByte(Protocol.VERSION)
        output.writeByte(type)
        output.writeInt(payload.size) // big-endian u32
        output.write(payload)
        output.flush()
    }
}

/**
 * Screen capture + hardware video encoder.
 *
 * Real implementation plan:
 *  - Allocate an input Surface from a MediaCodec configured as an H.264/H.265
 *    encoder (MediaFormat.MIMETYPE_VIDEO_AVC / _HEVC), with low-latency tuning:
 *    KEY_BITRATE_MODE = BITRATE_MODE_CBR, a short KEY_I_FRAME_INTERVAL, and on
 *    API 30+ KEY_LOW_LATENCY = 1.
 *  - Use the hidden SurfaceControl / DisplayManager APIs (reached via
 *    reflection because they are @hide) to mirror the main display onto that
 *    encoder input Surface.
 *  - Drain MediaCodec.dequeueOutputBuffer on a tight loop and emit each access
 *    unit as a VIDEO_FRAME packet; emit the codec-specific data (SPS/PPS/VPS)
 *    once as a VIDEO_CONFIG packet.
 */
class ScreenEncoder(private val codec: VideoCodec) {
    enum class VideoCodec { H264, H265 }

    fun start(sink: (Packet) -> Unit) {
        // TODO: configure MediaCodec encoder + SurfaceControl mirror.
        // TODO: emit VIDEO_CONFIG once, then a stream of VIDEO_FRAME packets.
        stub("ScreenEncoder.start(codec=$codec)")
    }

    fun stop() {
        // TODO: signal end-of-stream, release the MediaCodec and Surface.
        stub("ScreenEncoder.stop")
    }
}

/**
 * Remote input injection.
 *
 * Real implementation plan: obtain the hidden android.hardware.input.InputManager
 * singleton via reflection and call injectInputEvent(MotionEvent/KeyEvent,
 * INJECT_INPUT_EVENT_MODE_ASYNC). MOUSE_EVENT / TOUCH_EVENT payloads map to
 * MotionEvent (ACTION_DOWN/MOVE/UP, pointer coordinates), KEY_EVENT payloads map
 * to KeyEvent (keycode, action, meta-state from the modifiers bitmask).
 */
class InputInjector {
    fun inject(packet: Packet) {
        when (packet.type) {
            Protocol.MOUSE_EVENT -> stub("inject MouseEvent (${packet.payload.size} bytes)")
            Protocol.KEY_EVENT -> stub("inject KeyEvent (${packet.payload.size} bytes)")
            Protocol.TOUCH_EVENT -> stub("inject TouchEvent (${packet.payload.size} bytes)")
            else -> stub("inject: ignoring packet type 0x%02X".format(packet.type))
        }
        // TODO: InputManager.injectInputEvent(...) via reflection.
    }
}

/**
 * Dedicated fast file-transfer channel.
 *
 * Real implementation plan: a second socket (separate port from the control /
 * video channel) carrying FILE_META then a sequence of FILE_CHUNK packets, so a
 * large transfer never head-of-line-blocks the low-latency video stream. The
 * is_last flag on FILE_CHUNK terminates each transfer; an ACK flows back on the
 * control channel.
 */
class FileTransferServer(private val port: Int) {
    fun serve() {
        // TODO: accept() on a dedicated ServerSocket(port) and stream
        // FILE_META + FILE_CHUNK packets with back-pressure.
        stub("FileTransferServer.serve on port $port")
    }
}

/**
 * Virtual secondary display.
 *
 * Real implementation plan: DisplayManager.createVirtualDisplay(name, w, h, dpi,
 * surface, VIRTUAL_DISPLAY_FLAG_PUBLIC | _OWN_CONTENT_ONLY). The returned
 * display id can be handed to an app launch so a single app is sandboxed onto
 * its own off-device surface, which is then encoded exactly like the main one.
 */
class VirtualDisplayManager {
    fun createSecondaryDisplay(widthPx: Int, heightPx: Int, dpi: Int) {
        // TODO: DisplayManager.createVirtualDisplay(...) targeting an encoder Surface.
        stub("createSecondaryDisplay(${widthPx}x$heightPx @ ${dpi}dpi)")
    }
}

/**
 * Server entry point launched by `app_process`.
 *
 * Opens the control/video socket, performs the HANDSHAKE exchange, then fans the
 * incoming control packets out to the input injector while the encoder pushes
 * video frames back. File transfer and the virtual display are started on
 * demand. This is a skeleton: the accept loop and dispatch are real enough to
 * read, the framework calls are stubbed.
 */
object Server {
    private const val DEFAULT_PORT = 27183

    @JvmStatic
    fun main(args: Array<String>) {
        val port = args.firstOrNull()?.toIntOrNull() ?: DEFAULT_PORT
        log("FluxBridge server starting on port $port (protocol v${Protocol.VERSION})")

        val encoder = ScreenEncoder(ScreenEncoder.VideoCodec.H264)
        val input = InputInjector()
        val fileServer = FileTransferServer(port + 1)
        val displays = VirtualDisplayManager()

        ServerSocket(port).use { server ->
            while (true) {
                val client = server.accept()
                log("client connected: ${client.inetAddress}")
                handleClient(client, encoder, input, fileServer, displays)
            }
        }
    }

    private fun handleClient(
        client: Socket,
        encoder: ScreenEncoder,
        input: InputInjector,
        fileServer: FileTransferServer,
        displays: VirtualDisplayManager,
    ) {
        val codec = PacketCodec(client)
        try {
            // 1. Handshake: read the client capabilities, echo our own back.
            val hello = codec.read()
            require(hello.type == Protocol.HANDSHAKE) { "expected HANDSHAKE, got 0x%02X".format(hello.type) }
            codec.write(Protocol.HANDSHAKE, byteArrayOf())

            // 2. Start the video pipeline; frames are framed back to the client.
            encoder.start { packet -> codec.write(packet.type, packet.payload) }

            // 3. Optional: a dedicated channel + a virtual display for sandboxing.
            fileServer.serve()
            displays.createSecondaryDisplay(widthPx = 1080, heightPx = 2400, dpi = 420)

            // 4. Control loop: route inbound packets to the right subsystem.
            while (true) {
                val packet = codec.read()
                when (packet.type) {
                    Protocol.MOUSE_EVENT,
                    Protocol.KEY_EVENT,
                    Protocol.TOUCH_EVENT -> input.inject(packet)
                    Protocol.HEARTBEAT -> codec.write(Protocol.ACK, byteArrayOf())
                    else -> log("unhandled packet type 0x%02X".format(packet.type))
                }
            }
        } catch (e: IOException) {
            log("client disconnected: ${e.message}")
        } finally {
            encoder.stop()
            client.close()
        }
    }

    private fun log(message: String) = System.err.println("[fluxbridge-server] $message")
}

/** Marks a not-yet-implemented framework call in this skeleton. */
private fun stub(what: String) {
    System.err.println("[fluxbridge-server][stub] $what")
}
