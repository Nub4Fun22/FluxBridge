//! Connection skeleton for the FluxBridge desktop client.
//!
//! The desktop app connects to the Android capture server over TCP, performs a
//! [`PacketType::Handshake`] exchange using the core [`PacketCodec`], and then
//! streams protocol packets. This module provides:
//!
//! * [`ConnectionConfig`] describing the target host/port.
//! * [`Connection::connect`], which opens the socket and completes the
//!   handshake.
//! * A reader loop that frames packets off the stream and forwards every
//!   decoded [`Packet`] onto an `mpsc` channel the decode loop consumes.
//! * A documented reconnect-loop stub and an mDNS discovery helper that drives
//!   adb's own wireless-debugging mDNS browser (see [`discover_via_mdns`]).
//!
//! The implementation is deliberately `std`-only (blocking `TcpStream` on a
//! background thread) so the scaffold has no async runtime dependency.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use fluxbridge_core::protocol::{DecodeResult, Packet, PacketCodec, PacketType};
use fluxbridge_core::{CoreError, CoreResult};

/// Map a [`std::io::Error`] into a [`CoreError`] (core has no `From` impl for
/// `io::Error`, only for `ProtocolError`).
fn io_err(e: std::io::Error) -> CoreError {
    CoreError::Other(format!("io error: {e}"))
}

/// Default TCP port the Android capture server listens on.
pub const DEFAULT_PORT: u16 = 27183;
/// Default host used when none is configured (USB forward via adb lands here).
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// Where and how to connect to the device server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionConfig {
    /// Target host (IP or name).
    pub host: String,
    /// Target TCP port.
    pub port: u16,
    /// Socket read timeout; `None` blocks indefinitely.
    pub read_timeout: Option<Duration>,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        ConnectionConfig {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            read_timeout: Some(Duration::from_secs(30)),
        }
    }
}

impl ConnectionConfig {
    /// Build a config from a `host:port` string, falling back to defaults.
    pub fn from_target(target: &str) -> Self {
        let mut cfg = ConnectionConfig::default();
        if let Some((host, port)) = target.rsplit_once(':') {
            cfg.host = host.to_string();
            if let Ok(p) = port.parse::<u16>() {
                cfg.port = p;
            }
        } else if !target.is_empty() {
            cfg.host = target.to_string();
        }
        cfg
    }

    /// The `host:port` address string.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// An established connection to the device server.
///
/// Holds the TCP stream and the receiver end of the packet channel. The reader
/// thread owns the write-half clone it needs; callers read decoded packets from
/// [`Connection::packets`].
pub struct Connection {
    stream: TcpStream,
    packet_rx: Receiver<Packet>,
}

impl Connection {
    /// Connect to the configured server and complete the handshake.
    ///
    /// On success a background reader thread is spawned that frames packets off
    /// the socket and forwards them onto the channel returned by
    /// [`Connection::packets`].
    pub fn connect(config: &ConnectionConfig) -> CoreResult<Self> {
        let addr = config
            .address()
            .to_socket_addrs()
            .map_err(io_err)?
            .next()
            .ok_or_else(|| CoreError::Other(format!("could not resolve {}", config.address())))?;
        let stream = TcpStream::connect(addr).map_err(io_err)?;
        stream
            .set_read_timeout(config.read_timeout)
            .map_err(io_err)?;

        let mut conn_stream = stream.try_clone().map_err(io_err)?;
        // The handshake may read past the reply into the first data packet if
        // the peer coalesces them into one TCP segment. Those leftover bytes are
        // returned here and used to SEED the reader thread's buffer so the data
        // packet is not dropped. See the combined-segment regression test.
        let leftover = perform_handshake(&mut conn_stream)?;

        let (tx, rx) = mpsc::channel();
        let reader_stream = stream.try_clone().map_err(io_err)?;
        std::thread::Builder::new()
            .name("fluxbridge-conn-reader".into())
            .spawn(move || reader_loop(reader_stream, leftover, tx))
            .map_err(io_err)?;

        Ok(Connection {
            stream,
            packet_rx: rx,
        })
    }

    /// The receiving end of the decoded-packet channel.
    ///
    /// The decode loop (`decode_loop.rs`) pulls [`PacketType::VideoFrame`]
    /// packets from here; other consumers can match on the full stream.
    pub fn packets(&self) -> &Receiver<Packet> {
        &self.packet_rx
    }

    /// Send a packet to the device (input events, acks, heartbeats).
    pub fn send(&mut self, packet: &Packet) -> CoreResult<()> {
        let bytes = PacketCodec::encode(packet).map_err(CoreError::from)?;
        self.stream.write_all(&bytes).map_err(io_err)?;
        self.stream.flush().map_err(io_err)?;
        Ok(())
    }
}

/// Perform the client side of the handshake: send a `Handshake` packet and wait
/// for the server's `Handshake` reply.
///
/// Returns any bytes that were buffered PAST the handshake reply. When the peer
/// writes the reply and the first data packet in the same TCP segment, a single
/// `read` pulls both into the local buffer; the surplus is handed back to the
/// caller so it can seed the reader thread instead of being dropped.
///
/// The payload is intentionally empty in this scaffold; capability negotiation
/// (screen size, codecs, feature flags) will be added as the protocol grows.
fn perform_handshake(stream: &mut TcpStream) -> CoreResult<Vec<u8>> {
    let hello = Packet::new(PacketType::Handshake, Vec::new());
    let bytes = PacketCodec::encode(&hello).map_err(CoreError::from)?;
    stream.write_all(&bytes).map_err(io_err)?;
    stream.flush().map_err(io_err)?;

    let (reply, leftover) = read_one_packet(stream)?;
    if reply.packet_type != PacketType::Handshake {
        return Err(CoreError::Other(format!(
            "expected Handshake reply, got {:?}",
            reply.packet_type
        )));
    }
    Ok(leftover)
}

/// Read exactly one framed packet from a blocking stream.
///
/// Grows a buffer until [`PacketCodec::decode`] reports a complete packet,
/// handling the `Truncated` case by reading more bytes. Returns the decoded
/// packet together with any bytes buffered AFTER it (the start of the next
/// packet that shared a read with this one); callers must not discard them.
fn read_one_packet(stream: &mut TcpStream) -> CoreResult<(Packet, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let mut chunk = [0u8; 1024];
    loop {
        match PacketCodec::decode(&buf) {
            Ok(DecodeResult { packet, consumed }) => {
                let leftover = buf[consumed..].to_vec();
                return Ok((packet, leftover));
            }
            Err(fluxbridge_core::ProtocolError::Truncated { .. }) => {
                let n = stream.read(&mut chunk).map_err(io_err)?;
                if n == 0 {
                    return Err(CoreError::NotConnected);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) => return Err(CoreError::from(e)),
        }
    }
}

/// Background reader loop: frames packets off `stream` and forwards them to
/// `tx` until the socket closes or the receiver is dropped.
///
/// `seed` carries any bytes the handshake read past the reply (see
/// [`perform_handshake`]); the loop starts with them already in its buffer so a
/// data packet that shared the handshake's TCP segment is delivered, not lost.
fn reader_loop(mut stream: TcpStream, seed: Vec<u8>, tx: Sender<Packet>) {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    buf.extend_from_slice(&seed);
    let mut chunk = [0u8; 8 * 1024];
    loop {
        // Drain all complete packets currently buffered.
        loop {
            match PacketCodec::decode(&buf) {
                Ok(DecodeResult { packet, consumed }) => {
                    buf.drain(..consumed);
                    if tx.send(packet).is_err() {
                        return; // consumer gone
                    }
                }
                Err(fluxbridge_core::ProtocolError::Truncated { .. }) => break,
                Err(_) => {
                    // Corrupt stream: drop a byte and resync on the next magic.
                    if buf.is_empty() {
                        break;
                    }
                    buf.remove(0);
                }
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => return, // peer closed
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return,
        }
    }
}

/// Reconnect loop stub.
///
/// Phase 1 keeps reconnection manual. This helper documents the intended
/// behaviour: retry [`Connection::connect`] with capped exponential backoff
/// until it succeeds or `max_attempts` is reached. It is wired into the app's
/// supervisor task in a later milestone.
pub fn reconnect_with_backoff(
    config: &ConnectionConfig,
    max_attempts: u32,
) -> CoreResult<Connection> {
    let mut delay = Duration::from_millis(500);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match Connection::connect(config) {
            Ok(conn) => return Ok(conn),
            Err(e) if attempt >= max_attempts => return Err(e),
            Err(_) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        }
    }
}

/// Discover a wireless-debugging device on the LAN via mDNS.
///
/// This is implemented by driving adb's OWN mDNS browser rather than shipping a
/// second mDNS stack: it locates adb ([`crate::adb::locate_adb`]), runs
/// `adb mdns services` ([`crate::adb::AdbCommand::mdns_services`]), and returns
/// a [`ConnectionConfig`] for the first `_adb-tls-connect._tcp` endpoint found.
/// See [`crate::adb::parse_mdns_services`] for the parser (unit-tested against a
/// captured sample) and [`crate::devices`] for remembering a paired device so a
/// returning user auto-reconnects.
///
/// Note the returned config points at the DEVICE's discovered connect endpoint.
/// The mirroring pipeline does the actual `adb connect` and then tunnels the
/// scrcpy socket through `adb forward` to `127.0.0.1`; this helper exists so a
/// caller can surface the discovered address (and so a `None` honestly means
/// "nothing discovered / adb unavailable", never a left-over Phase 2 stub).
///
/// Returns `None` when adb is not installed, the mDNS query fails, or no
/// connect endpoint has been advertised yet (for example the phone's Wireless
/// debugging screen is not open). That is not an error: callers fall back to
/// the configured/default target or the pairing UX.
pub fn discover_via_mdns() -> Option<ConnectionConfig> {
    let adb_path = crate::adb::locate_adb().ok()?;
    let cmd = crate::adb::AdbCommand::with_path(adb_path, crate::adb::DeviceSelector::UsbDefault);
    let services = cmd.mdns_services().ok()?;
    let connect = services.into_iter().find(|s| s.is_connect())?;
    Some(ConnectionConfig {
        host: connect.host,
        port: connect.port,
        read_timeout: ConnectionConfig::default().read_timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_target_parses_host_and_port() {
        let cfg = ConnectionConfig::from_target("192.168.1.5:5555");
        assert_eq!(cfg.host, "192.168.1.5");
        assert_eq!(cfg.port, 5555);
    }

    #[test]
    fn config_from_target_defaults_port_when_missing() {
        let cfg = ConnectionConfig::from_target("myphone");
        assert_eq!(cfg.host, "myphone");
        assert_eq!(cfg.port, DEFAULT_PORT);
    }

    #[test]
    fn default_target_is_local_adb_forward() {
        let cfg = ConnectionConfig::default();
        assert_eq!(cfg.address(), "127.0.0.1:27183");
    }

    /// `discover_via_mdns` drives adb's real mDNS browser. On this headless CI
    /// host adb is not installed, so the call honestly returns `None` (adb
    /// unavailable), NOT because it is an unimplemented Phase 2 stub. The live
    /// discovery path (adb present, phone advertising) is verified by the user
    /// on Windows; the parser it depends on is unit-tested in `adb.rs` against
    /// a captured sample.
    ///
    /// The lookup reads process-global env (`PATH`, `ANDROID_HOME`, ...), so it
    /// goes through the shared env lock and strips those vars to force the
    /// adb-absent outcome deterministically, matching the other env-touching
    /// desktop tests.
    #[test]
    fn discover_via_mdns_returns_none_when_adb_absent() {
        crate::test_env::with_env_lock(|| {
            std::env::remove_var("ANDROID_HOME");
            std::env::remove_var("ANDROID_SDK_ROOT");
            std::env::set_var("PATH", "/nonexistent-path-xyz");
            std::env::remove_var("HOME");
            std::env::remove_var("LOCALAPPDATA");
            std::env::remove_var("XDG_DATA_HOME");
            assert!(
                discover_via_mdns().is_none(),
                "with adb absent, discovery yields None (no stub, no panic)"
            );
        });
    }

    /// End-to-end loopback test of the socket seam: handshake over the real
    /// `PacketCodec`, then a `VideoFrame` streamed through `run_decode_loop`.
    ///
    /// A `TcpListener` on `127.0.0.1:0` plays the device server. It reads the
    /// client's `Handshake`, replies with its own `Handshake`, then sends a
    /// single `VideoFrame`. The client connects via [`Connection::connect`]
    /// (which completes the handshake and spawns the reader thread), and the
    /// decode loop consumes the frame off the packet channel. This covers the
    /// framing-over-TCP path that the config-string unit tests do not touch.
    #[test]
    fn loopback_handshake_and_frame_through_decode_loop() {
        use crate::decode_loop::{run_decode_loop, StubDecoder};
        use fluxbridge_core::DecodedFrame;
        use std::net::TcpListener;
        use std::sync::mpsc as std_mpsc;

        let frame_payload = vec![0xA1u8, 0xB2, 0xC3, 0xD4];
        let server_payload = frame_payload.clone();

        // Bind to an ephemeral port so the test never collides with a real one.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");

        // Lets the client tell the server to close once it has the frame.
        let (done_tx, done_rx) = std_mpsc::channel::<()>();

        // Server side: accept one client, complete the handshake, stream a frame.
        let server = std::thread::spawn(move || {
            let (mut sock, _peer) = listener.accept().expect("accept client");

            // Read the client's Handshake and verify it frames correctly.
            let (hello, _rest) = read_one_packet(&mut sock).expect("read client handshake");
            assert_eq!(hello.packet_type, PacketType::Handshake);

            // Reply with our own Handshake and flush it on its own.
            let reply = PacketCodec::encode(&Packet::new(PacketType::Handshake, Vec::new()))
                .expect("encode handshake reply");
            sock.write_all(&reply).expect("write handshake reply");
            sock.flush().expect("flush handshake reply");

            // Pause so the handshake reply lands in its own read on the client.
            // `perform_handshake` reads one packet and discards any trailing
            // bytes in its local buffer, so the frame must arrive only after the
            // reader thread (spawned post-handshake) is the one reading.
            std::thread::sleep(Duration::from_millis(150));

            let frame = PacketCodec::encode(&Packet::new(PacketType::VideoFrame, server_payload))
                .expect("encode video frame");
            sock.write_all(&frame).expect("write video frame");
            sock.flush().expect("flush video frame");

            // Keep the socket open until signaled so the client reader drains
            // the frame before the peer closes.
            let _ = done_rx.recv_timeout(Duration::from_secs(5));
        });

        let config = ConnectionConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            read_timeout: Some(Duration::from_secs(5)),
        };
        let connection = Connection::connect(&config).expect("client connects + handshakes");

        // Drive the decode loop on a worker thread so the main thread can wait
        // on the decoded-frame channel with a timeout.
        let (frame_tx, frame_rx) = std_mpsc::channel::<DecodedFrame>();
        let decoder_thread = std::thread::spawn(move || {
            let mut decoder = StubDecoder::new();
            // Returns when the peer closes (reader thread drops the sender) or
            // the frame sink is gone.
            let _ = run_decode_loop(connection.packets(), &mut decoder, &frame_tx);
        });

        // Wait for the frame to flow all the way through the decode loop.
        let first = frame_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a decoded frame arrives through the loop");
        assert_eq!(first.data, frame_payload, "frame bytes survive the round trip");

        // Tell the server it can close now, then let both threads wind down.
        let _ = done_tx.send(());
        drop(frame_rx);
        server.join().expect("server thread");
        decoder_thread.join().expect("decoder thread");
    }

    /// Regression test for the deferred perform_handshake trailing-bytes bug.
    ///
    /// The server writes the handshake reply AND the first `VideoFrame` in a
    /// SINGLE `write` (one TCP segment), with no sleep in between. Before the
    /// fix, `perform_handshake` decoded the reply and dropped the trailing frame
    /// bytes left in its local buffer, so the frame never reached the decode
    /// loop. The fix returns those leftover bytes and seeds the reader thread
    /// with them. This test asserts the frame still arrives, specifically
    /// WITHOUT the 150ms sleep the loopback test uses to sidestep the race.
    #[test]
    fn handshake_reply_and_frame_in_one_segment_still_delivers_frame() {
        use crate::decode_loop::{run_decode_loop, StubDecoder};
        use fluxbridge_core::DecodedFrame;
        use std::net::TcpListener;
        use std::sync::mpsc as std_mpsc;

        let frame_payload = vec![0x11u8, 0x22, 0x33, 0x44, 0x55];
        let server_payload = frame_payload.clone();

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let (done_tx, done_rx) = std_mpsc::channel::<()>();

        let server = std::thread::spawn(move || {
            let (mut sock, _peer) = listener.accept().expect("accept client");
            let (hello, _rest) = read_one_packet(&mut sock).expect("read client handshake");
            assert_eq!(hello.packet_type, PacketType::Handshake);

            // Build handshake reply + first frame and write them together, so
            // both land in one read on the client (the combined-segment case).
            let mut combined =
                PacketCodec::encode(&Packet::new(PacketType::Handshake, Vec::new()))
                    .expect("encode handshake reply");
            let frame = PacketCodec::encode(&Packet::new(PacketType::VideoFrame, server_payload))
                .expect("encode video frame");
            combined.extend_from_slice(&frame);
            sock.write_all(&combined).expect("write combined segment");
            sock.flush().expect("flush combined segment");

            let _ = done_rx.recv_timeout(Duration::from_secs(5));
        });

        let config = ConnectionConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            read_timeout: Some(Duration::from_secs(5)),
        };
        let connection = Connection::connect(&config).expect("client connects + handshakes");

        let (frame_tx, frame_rx) = std_mpsc::channel::<DecodedFrame>();
        let decoder_thread = std::thread::spawn(move || {
            let mut decoder = StubDecoder::new();
            let _ = run_decode_loop(connection.packets(), &mut decoder, &frame_tx);
        });

        let first = frame_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the frame that shared the handshake segment must still arrive");
        assert_eq!(first.data, frame_payload, "coalesced frame bytes survive");

        let _ = done_tx.send(());
        drop(frame_rx);
        server.join().expect("server thread");
        decoder_thread.join().expect("decoder thread");
    }
}
