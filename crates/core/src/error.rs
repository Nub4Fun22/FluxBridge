//! Error types shared across the FluxBridge core.

use thiserror::Error;

/// Errors produced while framing or parsing the wire protocol.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// The leading magic bytes did not match `0xFB 0x01`.
    #[error("bad magic bytes: got {got:02X?}, expected [0xFB, 0x01]")]
    BadMagic {
        /// The two bytes that were found where the magic was expected.
        got: [u8; 2],
    },

    /// The protocol version byte is not supported by this build.
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u8),

    /// The packet-type byte did not map to a known [`crate::protocol::PacketType`].
    #[error("unknown packet type: 0x{0:02X}")]
    UnknownPacketType(u8),

    /// A typed payload was decoded from a packet of the wrong type.
    #[error("unexpected packet type: expected 0x{expected:02X}, got 0x{got:02X}")]
    UnexpectedPacketType {
        /// The type the caller expected.
        expected: u8,
        /// The type actually present on the packet.
        got: u8,
    },

    /// The buffer does not yet contain a complete packet.
    #[error("truncated packet: need {needed} bytes, have {got}")]
    Truncated {
        /// Number of bytes needed before the packet can be decoded.
        needed: usize,
        /// Number of bytes currently available.
        got: usize,
    },

    /// A payload or string exceeded the size that fits in its length field.
    #[error("payload too large to encode: {0} bytes")]
    PayloadTooLarge(usize),

    /// A string field was not valid UTF-8.
    #[error("invalid UTF-8 in string field")]
    InvalidUtf8,

    /// An underlying I/O error occurred while reading/writing bytes.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors produced by the OS-facing trait implementations and bridges.
#[derive(Debug, Error)]
pub enum CoreError {
    /// The requested operation is not implemented on this platform / stub.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// A protocol-level error occurred.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    /// A device or transport is not connected.
    #[error("not connected")]
    NotConnected,

    /// A generic, human-readable failure.
    #[error("{0}")]
    Other(String),
}

/// Convenience alias for results that fail with [`CoreError`].
pub type CoreResult<T> = Result<T, CoreError>;
