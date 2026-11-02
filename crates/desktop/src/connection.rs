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
//! * A documented reconnect-loop stub and an mDNS auto-pairing hook (Phase 2).
//!
//! The implementation is deliberately `std`-only (blocking `TcpStream` on a
//! background thread) so the scaffold has no async runtime dependency. The
//! `docs/ROADMAP.md` Phase 2 notes describe swapping in tokio when wireless
//! auto-docking lands.

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
        perform_handshake(&mut conn_stream)?;

        let (tx, rx) = mpsc::channel();
        let reader_stream = stream.try_clone().map_err(io_err)?;
        std::thread::Builder::new()
            .name("fluxbridge-conn-reader".into())
            .spawn(move || reader_loop(reader_stream, tx))
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
/// The payload is intentionally empty in this scaffold; capability negotiation
/// (screen size, codecs, feature flags) will be added as the protocol grows.
fn perform_handshake(stream: &mut TcpStream) -> CoreResult<()> {
    let hello = Packet::new(PacketType::Handshake, Vec::new());
    let bytes = PacketCodec::encode(&hello).map_err(CoreError::from)?;
    stream.write_all(&bytes).map_err(io_err)?;
    stream.flush().map_err(io_err)?;

    let reply = read_one_packet(stream)?;
    if reply.packet_type != PacketType::Handshake {
        return Err(CoreError::Other(format!(
            "expected Handshake reply, got {:?}",
            reply.packet_type
        )));
    }
    Ok(())
}

/// Read exactly one framed packet from a blocking stream.
///
/// Grows a buffer until [`PacketCodec::decode`] reports a complete packet,
/// handling the `Truncated` case by reading more bytes.
fn read_one_packet(stream: &mut TcpStream) -> CoreResult<Packet> {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let mut chunk = [0u8; 1024];
    loop {
        match PacketCodec::decode(&buf) {
            Ok(DecodeResult { packet, .. }) => return Ok(packet),
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
fn reader_loop(mut stream: TcpStream, tx: Sender<Packet>) {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
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

/// mDNS auto-pairing hook (Phase 2).
///
/// Wireless auto-docking will advertise/browse the `_fluxbridge._tcp.local`
/// service via mDNS so the desktop discovers phones on the LAN without manual
/// IP entry. This stub documents the integration point; the real resolver
/// (e.g. the `mdns-sd` crate) is added when Phase 2 begins. It currently
/// returns the configured default so the rest of the app can call it today.
pub fn discover_via_mdns() -> Option<ConnectionConfig> {
    // TODO(phase-2): browse `_fluxbridge._tcp.local`, resolve the first host,
    // and return its address. Returning None here means "fall back to the
    // configured/default target".
    None
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

    #[test]
    fn mdns_stub_returns_none() {
        assert!(discover_via_mdns().is_none());
    }
}
