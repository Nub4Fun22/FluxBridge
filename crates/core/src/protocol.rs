//! FluxBridge binary wire protocol.
//!
//! All control and media traffic between the desktop host and a connected
//! device is framed as fixed-header packets. The framing is deliberately simple
//! so that it can be implemented identically in the Rust host, the Kotlin
//! Android capture server and any future iOS bridge.
//!
//! # Packet layout
//!
//! ```text
//! +--------+--------+---------+-------------+------------------------+
//! | byte 0 | byte 1 | byte 2  | byte 3      | bytes 4..8             |
//! +--------+--------+---------+-------------+------------------------+
//! | 0xFB   | 0x01   | version | packet_type | payload length (u32 BE)|
//! +--------+--------+---------+-------------+------------------------+
//! | bytes 8..8+len : payload (packet-type specific, big-endian)      |
//! +------------------------------------------------------------------+
//! ```
//!
//! * Bytes 0 and 1 are the fixed magic `0xFB 0x01`.
//! * Byte 2 is the protocol `version` (currently [`PROTOCOL_VERSION`]).
//! * Byte 3 is the [`PacketType`] discriminant.
//! * Bytes 4..8 are the payload length as a big-endian `u32`.
//! * The remaining `length` bytes are the payload.
//!
//! The total fixed header size is [`HEADER_LEN`] (8 bytes). Multi-byte integers
//! in both the header and the payloads are encoded big-endian (network order).
//! Strings are length-prefixed with a big-endian `u16` byte count followed by
//! UTF-8 bytes.

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Cursor, Read};

use crate::error::ProtocolError;

/// First magic byte of every FluxBridge packet.
pub const MAGIC_0: u8 = 0xFB;
/// Second magic byte of every FluxBridge packet.
pub const MAGIC_1: u8 = 0x01;
/// Current wire protocol version.
pub const PROTOCOL_VERSION: u8 = 1;
/// Size in bytes of the fixed packet header (magic + version + type + length).
pub const HEADER_LEN: usize = 8;

/// The kind of payload a [`Packet`] carries.
///
/// The discriminant is serialized as a single byte (header byte 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PacketType {
    /// Connection setup / capability exchange.
    Handshake = 0x01,
    /// Video stream configuration (codec, resolution, parameters).
    VideoConfig = 0x02,
    /// A single encoded video frame.
    VideoFrame = 0x03,
    /// A pointer / mouse event destined for the device.
    MouseEvent = 0x10,
    /// A keyboard event destined for the device.
    KeyEvent = 0x11,
    /// A touch event destined for the device.
    TouchEvent = 0x12,
    /// Clipboard text synchronization.
    ClipboardText = 0x20,
    /// Clipboard image synchronization.
    ClipboardImage = 0x21,
    /// A chunk of a file transfer.
    FileChunk = 0x30,
    /// Metadata describing a file transfer.
    FileMeta = 0x31,
    /// A reverse drag-and-drop "drag entered the host" notification.
    DragEnter = 0x40,
    /// A reverse drag-and-drop "dropped on host" notification.
    DragDrop = 0x41,
    /// Battery / signal status update.
    Battery = 0x50,
    /// Acknowledgement of a previously received packet.
    Ack = 0x60,
    /// Keep-alive heartbeat.
    Heartbeat = 0x61,
}

impl PacketType {
    /// Convert a raw byte into a [`PacketType`], or `None` if unknown.
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x01 => PacketType::Handshake,
            0x02 => PacketType::VideoConfig,
            0x03 => PacketType::VideoFrame,
            0x10 => PacketType::MouseEvent,
            0x11 => PacketType::KeyEvent,
            0x12 => PacketType::TouchEvent,
            0x20 => PacketType::ClipboardText,
            0x21 => PacketType::ClipboardImage,
            0x30 => PacketType::FileChunk,
            0x31 => PacketType::FileMeta,
            0x40 => PacketType::DragEnter,
            0x41 => PacketType::DragDrop,
            0x50 => PacketType::Battery,
            0x60 => PacketType::Ack,
            0x61 => PacketType::Heartbeat,
            _ => return None,
        })
    }
}

/// A fully framed FluxBridge packet (header + raw payload bytes).
///
/// The payload is kept as opaque bytes at this layer; strongly typed payload
/// structs such as [`MouseEvent`] provide their own encode/decode helpers and
/// are wrapped into a [`Packet`] via [`Packet::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Protocol version from the header.
    pub version: u8,
    /// The packet type.
    pub packet_type: PacketType,
    /// Raw payload bytes.
    pub payload: Vec<u8>,
}

impl Packet {
    /// Build a packet with the current [`PROTOCOL_VERSION`].
    pub fn new(packet_type: PacketType, payload: Vec<u8>) -> Self {
        Packet {
            version: PROTOCOL_VERSION,
            packet_type,
            payload,
        }
    }

    /// Serialize the packet (header + payload) into a byte vector.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let len: u32 = self
            .payload
            .len()
            .try_into()
            .map_err(|_| ProtocolError::PayloadTooLarge(self.payload.len()))?;
        let mut buf = Vec::with_capacity(HEADER_LEN + self.payload.len());
        buf.push(MAGIC_0);
        buf.push(MAGIC_1);
        buf.push(self.version);
        buf.push(self.packet_type as u8);
        buf.write_u32::<BigEndian>(len)?;
        buf.extend_from_slice(&self.payload);
        Ok(buf)
    }
}

/// Stateless framing codec for [`Packet`] values.
///
/// [`PacketCodec::decode`] reads exactly one packet from the front of a buffer
/// and reports how many bytes were consumed, so callers can drive it over a
/// growing stream buffer.
pub struct PacketCodec;

/// Result of a successful [`PacketCodec::decode`]: the packet plus the number
/// of bytes consumed from the input buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeResult {
    /// The decoded packet.
    pub packet: Packet,
    /// Number of bytes consumed from the input.
    pub consumed: usize,
}

impl PacketCodec {
    /// Encode a packet into bytes. Convenience wrapper over [`Packet::encode`].
    pub fn encode(packet: &Packet) -> Result<Vec<u8>, ProtocolError> {
        packet.encode()
    }

    /// Decode a single packet from the front of `buf`.
    ///
    /// # Errors
    /// * [`ProtocolError::BadMagic`] when the magic bytes do not match.
    /// * [`ProtocolError::Truncated`] when the buffer does not yet hold a full
    ///   header or the declared payload length.
    /// * [`ProtocolError::UnknownPacketType`] when the type byte is not a known
    ///   [`PacketType`].
    /// * [`ProtocolError::UnsupportedVersion`] when the version is not supported.
    pub fn decode(buf: &[u8]) -> Result<DecodeResult, ProtocolError> {
        if buf.len() < HEADER_LEN {
            return Err(ProtocolError::Truncated {
                needed: HEADER_LEN,
                got: buf.len(),
            });
        }
        if buf[0] != MAGIC_0 || buf[1] != MAGIC_1 {
            return Err(ProtocolError::BadMagic {
                got: [buf[0], buf[1]],
            });
        }
        let version = buf[2];
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let type_byte = buf[3];
        let packet_type = PacketType::from_u8(type_byte)
            .ok_or(ProtocolError::UnknownPacketType(type_byte))?;
        let mut len_cursor = Cursor::new(&buf[4..HEADER_LEN]);
        let payload_len = len_cursor.read_u32::<BigEndian>()? as usize;
        let total = HEADER_LEN + payload_len;
        if buf.len() < total {
            return Err(ProtocolError::Truncated {
                needed: total,
                got: buf.len(),
            });
        }
        let payload = buf[HEADER_LEN..total].to_vec();
        Ok(DecodeResult {
            packet: Packet {
                version,
                packet_type,
                payload,
            },
            consumed: total,
        })
    }
}

/// Write a length-prefixed (`u16` BE) UTF-8 string to `out`.
fn write_string(out: &mut Vec<u8>, s: &str) -> Result<(), ProtocolError> {
    let bytes = s.as_bytes();
    let len: u16 = bytes
        .len()
        .try_into()
        .map_err(|_| ProtocolError::PayloadTooLarge(bytes.len()))?;
    out.write_u16::<BigEndian>(len)?;
    out.extend_from_slice(bytes);
    Ok(())
}

/// Read a length-prefixed (`u16` BE) UTF-8 string from `cursor`.
fn read_string(cursor: &mut Cursor<&[u8]>) -> Result<String, ProtocolError> {
    let len = cursor.read_u16::<BigEndian>()? as usize;
    let mut bytes = vec![0u8; len];
    cursor.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|_| ProtocolError::InvalidUtf8)
}

/// Trait implemented by strongly typed payloads so they can be framed.
pub trait Payload: Sized {
    /// The [`PacketType`] this payload maps to.
    const PACKET_TYPE: PacketType;

    /// Serialize the payload body (without the packet header).
    fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError>;

    /// Deserialize the payload body (without the packet header).
    fn decode_payload(bytes: &[u8]) -> Result<Self, ProtocolError>;

    /// Frame this payload into a complete [`Packet`].
    fn to_packet(&self) -> Result<Packet, ProtocolError> {
        Ok(Packet::new(Self::PACKET_TYPE, self.encode_payload()?))
    }

    /// Extract and decode this payload from a [`Packet`].
    fn from_packet(packet: &Packet) -> Result<Self, ProtocolError> {
        if packet.packet_type != Self::PACKET_TYPE {
            return Err(ProtocolError::UnexpectedPacketType {
                expected: Self::PACKET_TYPE as u8,
                got: packet.packet_type as u8,
            });
        }
        Self::decode_payload(&packet.payload)
    }
}

/// A pointer / mouse event forwarded to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// X coordinate in device pixels.
    pub x: u16,
    /// Y coordinate in device pixels.
    pub y: u16,
    /// Button identifier (0 = none/move, 1 = left, 2 = right, 3 = middle).
    pub button: u8,
    /// Action (0 = move, 1 = down, 2 = up, 3 = scroll).
    pub action: u8,
    /// Scroll delta (signed), meaningful when `action` is scroll.
    pub scroll: i16,
}

impl Payload for MouseEvent {
    const PACKET_TYPE: PacketType = PacketType::MouseEvent;

    fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut out = Vec::with_capacity(8);
        out.write_u16::<BigEndian>(self.x)?;
        out.write_u16::<BigEndian>(self.y)?;
        out.write_u8(self.button)?;
        out.write_u8(self.action)?;
        out.write_i16::<BigEndian>(self.scroll)?;
        Ok(out)
    }

    fn decode_payload(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut c = Cursor::new(bytes);
        let ev = MouseEvent {
            x: c.read_u16::<BigEndian>()?,
            y: c.read_u16::<BigEndian>()?,
            button: c.read_u8()?,
            action: c.read_u8()?,
            scroll: c.read_i16::<BigEndian>()?,
        };
        Ok(ev)
    }
}

/// A keyboard event forwarded to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// Platform-independent key code.
    pub keycode: u32,
    /// Hardware scan code.
    pub scancode: u32,
    /// Action (0 = up, 1 = down, 2 = repeat).
    pub action: u8,
    /// Modifier bitmask (shift/ctrl/alt/meta).
    pub modifiers: u16,
}

impl Payload for KeyEvent {
    const PACKET_TYPE: PacketType = PacketType::KeyEvent;

    fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut out = Vec::with_capacity(11);
        out.write_u32::<BigEndian>(self.keycode)?;
        out.write_u32::<BigEndian>(self.scancode)?;
        out.write_u8(self.action)?;
        out.write_u16::<BigEndian>(self.modifiers)?;
        Ok(out)
    }

    fn decode_payload(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut c = Cursor::new(bytes);
        let ev = KeyEvent {
            keycode: c.read_u32::<BigEndian>()?,
            scancode: c.read_u32::<BigEndian>()?,
            action: c.read_u8()?,
            modifiers: c.read_u16::<BigEndian>()?,
        };
        Ok(ev)
    }
}

/// A touch event forwarded to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchEvent {
    /// Pointer identifier for multi-touch tracking.
    pub pointer_id: u8,
    /// X coordinate in device pixels.
    pub x: u16,
    /// Y coordinate in device pixels.
    pub y: u16,
    /// Action (0 = down, 1 = up, 2 = move, 3 = cancel).
    pub action: u8,
    /// Pressure (0..=65535 normalized).
    pub pressure: u16,
}

impl Payload for TouchEvent {
    const PACKET_TYPE: PacketType = PacketType::TouchEvent;

    fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut out = Vec::with_capacity(8);
        out.write_u8(self.pointer_id)?;
        out.write_u16::<BigEndian>(self.x)?;
        out.write_u16::<BigEndian>(self.y)?;
        out.write_u8(self.action)?;
        out.write_u16::<BigEndian>(self.pressure)?;
        Ok(out)
    }

    fn decode_payload(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut c = Cursor::new(bytes);
        let ev = TouchEvent {
            pointer_id: c.read_u8()?,
            x: c.read_u16::<BigEndian>()?,
            y: c.read_u16::<BigEndian>()?,
            action: c.read_u8()?,
            pressure: c.read_u16::<BigEndian>()?,
        };
        Ok(ev)
    }
}

/// Metadata describing a file being transferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// Unique transfer identifier.
    pub file_id: u32,
    /// File name (no path separators expected).
    pub name: String,
    /// Total file size in bytes.
    pub size: u64,
    /// MIME type string.
    pub mime: String,
}

impl Payload for FileMeta {
    const PACKET_TYPE: PacketType = PacketType::FileMeta;

    fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut out = Vec::new();
        out.write_u32::<BigEndian>(self.file_id)?;
        out.write_u64::<BigEndian>(self.size)?;
        write_string(&mut out, &self.name)?;
        write_string(&mut out, &self.mime)?;
        Ok(out)
    }

    fn decode_payload(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut c = Cursor::new(bytes);
        let file_id = c.read_u32::<BigEndian>()?;
        let size = c.read_u64::<BigEndian>()?;
        let name = read_string(&mut c)?;
        let mime = read_string(&mut c)?;
        Ok(FileMeta {
            file_id,
            name,
            size,
            mime,
        })
    }
}

/// A chunk of a file transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChunk {
    /// Transfer identifier matching a [`FileMeta`].
    pub file_id: u32,
    /// Sequence number of this chunk.
    pub seq: u32,
    /// Whether this is the final chunk of the transfer.
    pub is_last: bool,
    /// Raw chunk bytes.
    pub data: Vec<u8>,
}

impl Payload for FileChunk {
    const PACKET_TYPE: PacketType = PacketType::FileChunk;

    fn encode_payload(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut out = Vec::with_capacity(9 + self.data.len());
        out.write_u32::<BigEndian>(self.file_id)?;
        out.write_u32::<BigEndian>(self.seq)?;
        out.write_u8(u8::from(self.is_last))?;
        out.extend_from_slice(&self.data);
        Ok(out)
    }

    fn decode_payload(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut c = Cursor::new(bytes);
        let file_id = c.read_u32::<BigEndian>()?;
        let seq = c.read_u32::<BigEndian>()?;
        let is_last = c.read_u8()? != 0;
        let pos = c.position() as usize;
        let data = bytes[pos..].to_vec();
        Ok(FileChunk {
            file_id,
            seq,
            is_last,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_event_round_trip() {
        let ev = MouseEvent {
            x: 1280,
            y: 720,
            button: 1,
            action: 2,
            scroll: -3,
        };
        let packet = ev.to_packet().unwrap();
        let bytes = PacketCodec::encode(&packet).unwrap();
        let decoded = PacketCodec::decode(&bytes).unwrap();
        assert_eq!(decoded.consumed, bytes.len());
        assert_eq!(decoded.packet.packet_type, PacketType::MouseEvent);
        let back = MouseEvent::from_packet(&decoded.packet).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn key_event_round_trip() {
        let ev = KeyEvent {
            keycode: 65,
            scancode: 0x1E,
            action: 1,
            modifiers: 0b0000_0011,
        };
        let packet = ev.to_packet().unwrap();
        let bytes = PacketCodec::encode(&packet).unwrap();
        let back = KeyEvent::from_packet(&PacketCodec::decode(&bytes).unwrap().packet).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn touch_event_round_trip() {
        let ev = TouchEvent {
            pointer_id: 2,
            x: 100,
            y: 200,
            action: 0,
            pressure: 40000,
        };
        let bytes = PacketCodec::encode(&ev.to_packet().unwrap()).unwrap();
        let back = TouchEvent::from_packet(&PacketCodec::decode(&bytes).unwrap().packet).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn file_meta_round_trip() {
        let meta = FileMeta {
            file_id: 7,
            name: "photo.png".to_string(),
            size: 1_048_576,
            mime: "image/png".to_string(),
        };
        let bytes = PacketCodec::encode(&meta.to_packet().unwrap()).unwrap();
        let back = FileMeta::from_packet(&PacketCodec::decode(&bytes).unwrap().packet).unwrap();
        assert_eq!(meta, back);
    }

    #[test]
    fn file_chunk_round_trip() {
        let chunk = FileChunk {
            file_id: 7,
            seq: 42,
            is_last: true,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let bytes = PacketCodec::encode(&chunk.to_packet().unwrap()).unwrap();
        let back = FileChunk::from_packet(&PacketCodec::decode(&bytes).unwrap().packet).unwrap();
        assert_eq!(chunk, back);
    }

    #[test]
    fn empty_payload_packet_types_round_trip() {
        for pt in [
            PacketType::Handshake,
            PacketType::VideoConfig,
            PacketType::VideoFrame,
            PacketType::ClipboardText,
            PacketType::ClipboardImage,
            PacketType::DragEnter,
            PacketType::DragDrop,
            PacketType::Battery,
            PacketType::Ack,
            PacketType::Heartbeat,
        ] {
            let packet = Packet::new(pt, vec![0xDE, 0xAD, 0xBE, 0xEF]);
            let bytes = PacketCodec::encode(&packet).unwrap();
            let decoded = PacketCodec::decode(&bytes).unwrap();
            assert_eq!(decoded.packet, packet);
            assert_eq!(decoded.consumed, bytes.len());
        }
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = PacketCodec::encode(&Packet::new(PacketType::Ack, vec![])).unwrap();
        bytes[0] = 0x00;
        match PacketCodec::decode(&bytes) {
            Err(ProtocolError::BadMagic { got }) => assert_eq!(got, [0x00, MAGIC_1]),
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_packet_type() {
        let mut bytes = PacketCodec::encode(&Packet::new(PacketType::Ack, vec![])).unwrap();
        bytes[3] = 0x7F; // not a known type
        match PacketCodec::decode(&bytes) {
            Err(ProtocolError::UnknownPacketType(t)) => assert_eq!(t, 0x7F),
            other => panic!("expected UnknownPacketType, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut bytes = PacketCodec::encode(&Packet::new(PacketType::Ack, vec![])).unwrap();
        bytes[2] = 0xFF;
        match PacketCodec::decode(&bytes) {
            Err(ProtocolError::UnsupportedVersion(v)) => assert_eq!(v, 0xFF),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn rejects_truncated_header() {
        let bytes = [MAGIC_0, MAGIC_1, PROTOCOL_VERSION];
        match PacketCodec::decode(&bytes) {
            Err(ProtocolError::Truncated { needed, got }) => {
                assert_eq!(needed, HEADER_LEN);
                assert_eq!(got, 3);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn rejects_truncated_payload() {
        let mut bytes = PacketCodec::encode(&Packet::new(
            PacketType::FileChunk,
            vec![1, 2, 3, 4, 5, 6, 7, 8],
        ))
        .unwrap();
        // Drop the last 3 payload bytes so the declared length exceeds the buffer.
        bytes.truncate(bytes.len() - 3);
        match PacketCodec::decode(&bytes) {
            Err(ProtocolError::Truncated { needed, got }) => {
                assert!(needed > got);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn big_endian_length_encoding() {
        // Payload of 300 bytes exercises the high byte of the length field.
        let payload = vec![0xAB; 300];
        let bytes = PacketCodec::encode(&Packet::new(PacketType::VideoFrame, payload)).unwrap();
        // Length field is at bytes 4..8, big-endian. 300 = 0x0000_012C.
        assert_eq!(&bytes[4..8], &[0x00, 0x00, 0x01, 0x2C]);
        let decoded = PacketCodec::decode(&bytes).unwrap();
        assert_eq!(decoded.packet.payload.len(), 300);
    }

    #[test]
    fn decode_reports_consumed_for_concatenated_packets() {
        let a = PacketCodec::encode(&Packet::new(PacketType::Heartbeat, vec![])).unwrap();
        let b = PacketCodec::encode(&Packet::new(PacketType::Ack, vec![9, 9])).unwrap();
        let mut stream = a.clone();
        stream.extend_from_slice(&b);
        let first = PacketCodec::decode(&stream).unwrap();
        assert_eq!(first.consumed, a.len());
        let second = PacketCodec::decode(&stream[first.consumed..]).unwrap();
        assert_eq!(second.packet.packet_type, PacketType::Ack);
        assert_eq!(second.packet.payload, vec![9, 9]);
    }
}
