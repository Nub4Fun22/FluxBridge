//! scrcpy stream and control protocol (the shipped v1 transport).
//!
//! FluxBridge v1 drives the upstream scrcpy server (see [`crate::adb`]), so the
//! sockets exposed on `localabstract:scrcpy` do NOT speak the FluxBridge 0xFB
//! [`fluxbridge_core::protocol`] framing. They speak scrcpy's own framing, which
//! this module implements:
//!
//! ## Video socket framing
//!
//! When the server connects (reverse) or is connected to (forward), the video
//! socket begins with a metadata header, then a stream of length-prefixed H.264
//! access units:
//!
//! ```text
//! device name : 64 bytes, NUL-padded UTF-8            (once)
//! codec id    : u32 BE  (e.g. "h264" fourcc as a u32) (once, scrcpy >= 2.0)
//! width       : u32 BE                                (once)
//! height      : u32 BE                                (once)
//! --- then repeating, per access unit: ---
//! pts/flags   : u64 BE  (top bits are config/keyframe flags)
//! packet len  : u32 BE
//! payload     : <packet len> bytes of H.264
//! ```
//!
//! [`parse_video_header`] reads the one-shot header and [`read_av_packet`] pulls
//! one access unit; both operate on any [`std::io::Read`] so they are tested
//! against synthetic byte streams with no device.
//!
//! ## Control socket framing
//!
//! Control messages are sent to the device to inject input. Each message starts
//! with a one-byte type. FluxBridge implements and exact-byte tests the two that
//! matter for pointer and keyboard control:
//!
//! * `INJECT_KEYCODE` (type 0): action u8, keycode u32 BE, repeat u32 BE,
//!   metastate u32 BE.
//! * `INJECT_TOUCH_EVENT` (type 2): action u8, pointer id u64 BE, x i32 BE,
//!   y i32 BE, screen width u16 BE, screen height u16 BE, pressure u16 BE,
//!   action button u32 BE, buttons u32 BE.
//!
//! The FluxBridge-native [`fluxbridge_core::protocol`] events
//! ([`MouseEvent`](fluxbridge_core::protocol::MouseEvent) /
//! [`KeyEvent`](fluxbridge_core::protocol::KeyEvent) /
//! [`TouchEvent`](fluxbridge_core::protocol::TouchEvent)) are mapped into these
//! messages so the existing [`fluxbridge_core::InputSink`] abstraction drives
//! scrcpy unchanged. The FluxBridge 0xFB `PacketCodec` path stays intact for any
//! future FluxBridge-native server (see `android-server/Server.kt`), but it is
//! not used on scrcpy sockets.

use std::io::{self, Read, Write};
use std::sync::mpsc::Sender;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use fluxbridge_core::protocol::{KeyEvent, MouseEvent, TouchEvent};
use fluxbridge_core::{CoreResult, DecodedFrame, VideoConfig, VideoDecoder};

/// scrcpy control message type: inject a keycode.
pub const CTRL_INJECT_KEYCODE: u8 = 0;
/// scrcpy control message type: inject a touch event.
pub const CTRL_INJECT_TOUCH: u8 = 2;

/// scrcpy key/motion action: down / press.
pub const ACTION_DOWN: u8 = 0;
/// scrcpy key/motion action: up / release.
pub const ACTION_UP: u8 = 1;
/// scrcpy motion action: move.
pub const ACTION_MOVE: u8 = 2;

/// Fixed width of the device-name field at the head of the video socket.
pub const DEVICE_NAME_LEN: usize = 64;

/// The one-shot metadata header at the start of the scrcpy video socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoHeader {
    /// Device model name (trimmed of NUL padding).
    pub device_name: String,
    /// Codec fourcc as a big-endian u32 (for example `h264`).
    pub codec_id: u32,
    /// Stream width in pixels.
    pub width: u32,
    /// Stream height in pixels.
    pub height: u32,
}

/// A single scrcpy access unit (one H.264 NAL run) read off the video socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvPacket {
    /// Presentation timestamp in microseconds (flags masked off).
    pub pts_us: u64,
    /// Whether this packet carries codec config (SPS/PPS) rather than a frame.
    pub is_config: bool,
    /// Whether this packet is a keyframe.
    pub is_keyframe: bool,
    /// Encoded H.264 bytes.
    pub data: Vec<u8>,
}

/// scrcpy sets the top bit of the 64-bit pts field for config packets.
const PACKET_FLAG_CONFIG: u64 = 1u64 << 63;
/// scrcpy sets the next bit for keyframes.
const PACKET_FLAG_KEY_FRAME: u64 = 1u64 << 62;
/// Mask of the real pts bits (everything below the two flag bits).
const PACKET_PTS_MASK: u64 = (1u64 << 62) - 1;

/// Build the scrcpy fourcc `u32` for the four-character codec string, e.g.
/// `"h264"`. Used by tests and header construction.
pub fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

/// Read and parse the one-shot scrcpy video metadata header.
pub fn parse_video_header<R: Read>(r: &mut R) -> io::Result<VideoHeader> {
    let mut name = [0u8; DEVICE_NAME_LEN];
    r.read_exact(&mut name)?;
    let end = name.iter().position(|&b| b == 0).unwrap_or(DEVICE_NAME_LEN);
    let device_name = String::from_utf8_lossy(&name[..end]).into_owned();
    let codec_id = r.read_u32::<BigEndian>()?;
    let width = r.read_u32::<BigEndian>()?;
    let height = r.read_u32::<BigEndian>()?;
    Ok(VideoHeader {
        device_name,
        codec_id,
        width,
        height,
    })
}

/// The outcome of attempting to read one access unit at a packet boundary.
///
/// Distinguishing a read timeout from end-of-stream matters for the cooperative
/// stop: a short socket read timeout is set so the stream loop wakes to observe
/// a Stop request, and a timeout that fires at a packet boundary (no bytes of
/// the next packet consumed yet) is NOT an error or end of stream, it is just a
/// wake so the loop can re-check the cancel flag and keep going.
#[derive(Debug)]
pub enum PacketRead {
    /// A full access unit was read.
    Packet(AvPacket),
    /// The stream ended cleanly at a packet boundary.
    Eof,
    /// The read timed out at a packet boundary with no bytes consumed. The
    /// caller should re-check its cancel flag and retry. Only produced when the
    /// reader has a read timeout set (the live socket); the in-memory test
    /// streams never time out.
    TimedOut,
}

/// Whether an I/O error is a read-timeout wake (`WouldBlock` on Unix,
/// `TimedOut` on Windows) rather than a real failure.
fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Read one length-prefixed access unit from the video socket.
///
/// Returns [`PacketRead::Eof`] cleanly at end of stream (a zero-byte read where
/// the next packet header would start) and [`PacketRead::TimedOut`] when a read
/// timeout fires at a packet boundary before any byte of the next packet is
/// consumed (so the caller can wake, re-check cancellation, and retry). Once the
/// first bytes of a packet header have been read, a timeout mid-packet is still
/// an error (a truncated packet), since partial framing cannot be resumed: a
/// split pts-header read (some but not all 8 bytes consumed before the timeout)
/// surfaces as an `UnexpectedEof` error, NOT as [`PacketRead::TimedOut`]. The
/// len and payload reads after the header likewise propagate a timeout as an
/// error.
pub fn read_av_packet<R: Read>(r: &mut R) -> io::Result<PacketRead> {
    let mut pts_buf = [0u8; 8];
    match read_full_or_eof(r, &mut pts_buf)? {
        HeaderRead::Full => {}
        HeaderRead::Eof => return Ok(PacketRead::Eof),
        // A timeout with ZERO bytes consumed is a boundary wake, not a failure:
        // the loop re-checks its cancel flag and retries.
        HeaderRead::TimedOutAtBoundary => return Ok(PacketRead::TimedOut),
        // A timeout AFTER some header bytes were already read splits the pts
        // header. Partial framing cannot be resumed (re-reading would desync the
        // stream), so this is a truncated-packet error, never a wake.
        HeaderRead::TimedOutMidHeader(read) => {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("scrcpy packet header timed out after {read} of 8 bytes"),
            ))
        }
    }
    let pts_flags = u64::from_be_bytes(pts_buf);
    let len = r.read_u32::<BigEndian>()? as usize;
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok(PacketRead::Packet(AvPacket {
        pts_us: pts_flags & PACKET_PTS_MASK,
        is_config: pts_flags & PACKET_FLAG_CONFIG != 0,
        is_keyframe: pts_flags & PACKET_FLAG_KEY_FRAME != 0,
        data,
    }))
}

/// The outcome of filling a packet-header buffer, distinguishing a timeout that
/// fired at a packet boundary (no bytes consumed, a safe wake) from one that
/// fired after part of the header was already read (a split header that cannot
/// be resumed).
#[derive(Debug, PartialEq, Eq)]
enum HeaderRead {
    /// The buffer was filled completely.
    Full,
    /// The stream ended cleanly before any byte was read.
    Eof,
    /// A read timeout fired with zero bytes consumed (a packet boundary).
    TimedOutAtBoundary,
    /// A read timeout fired after `0 < n < buf.len()` bytes were consumed,
    /// splitting the header. Carries the byte count for the error message.
    TimedOutMidHeader(usize),
}

/// Read exactly `buf.len()` bytes, reporting how the read terminated.
///
/// Returns [`HeaderRead::Full`] on a complete read, [`HeaderRead::Eof`] when the
/// stream ends cleanly before any byte is read, and an error on a partial read
/// that hits EOF mid-buffer (truncated packet). A read timeout is surfaced
/// structurally so the caller can tell a zero-progress boundary timeout
/// ([`HeaderRead::TimedOutAtBoundary`]) apart from a timeout that already
/// consumed part of the header ([`HeaderRead::TimedOutMidHeader`]); only the
/// former is a safe wake, since a split header cannot be resumed without
/// desyncing the stream.
fn read_full_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<HeaderRead> {
    let mut read = 0;
    while read < buf.len() {
        match r.read(&mut buf[read..]) {
            Ok(0) => {
                if read == 0 {
                    return Ok(HeaderRead::Eof);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated scrcpy packet header",
                ));
            }
            Ok(n) => read += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(ref e) if is_timeout(e) => {
                if read == 0 {
                    return Ok(HeaderRead::TimedOutAtBoundary);
                }
                return Ok(HeaderRead::TimedOutMidHeader(read));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(HeaderRead::Full)
}

/// Consume the leading dummy byte scrcpy writes in tunnel-forward mode.
///
/// When the server is launched with `tunnel_forward=true` (the FluxBridge
/// default, see [`crate::adb::ServerLaunchOptions`]), scrcpy writes a single
/// `0x00` byte on the video socket BEFORE the metadata header. It exists so the
/// forwarding client can tell a successful device connection apart from adb
/// accepting the forward while no device is listening. The client must read and
/// discard exactly one byte before [`parse_video_header`]; skipping this makes
/// the header parse off by one. In reverse-tunnel mode no dummy byte is sent,
/// so this is only called when `tunnel_forward` is set.
pub fn consume_dummy_byte<R: Read>(r: &mut R) -> io::Result<()> {
    let mut one = [0u8; 1];
    r.read_exact(&mut one)?;
    Ok(())
}

/// Drive a [`VideoDecoder`] from a scrcpy video stream, emitting decoded frames.
///
/// Given any [`Read`] that yields a scrcpy video stream (optionally preceded by
/// the tunnel-forward dummy byte), this:
///
/// 1. consumes the dummy byte when `tunnel_forward` is true;
/// 2. parses the one-shot [`VideoHeader`] and configures `decoder` with the
///    device resolution from it;
/// 3. loops [`read_av_packet`], treating `is_config` access units as decoder
///    configuration (SPS/PPS extradata) and every other access unit as a frame
///    to decode;
/// 4. pushes each non-empty [`DecodedFrame`] onto `frame_sink`.
///
/// It returns `Ok(header)` when the stream ends cleanly (so the caller can log
/// the device/resolution that was streamed) and an error if the header cannot
/// be parsed, an access unit is truncated, or the decoder fails. A dropped
/// `frame_sink` receiver (UI closed) ends the loop cleanly, mirroring
/// [`crate::decode_loop::run_decode_loop`].
///
/// This is the SHIPPED v1 video path. It is deliberately generic over `Read`
/// and [`VideoDecoder`] so it is exercised headlessly against a synthetic
/// in-memory stream and a fake decoder (see the module tests); the real socket
/// and FFmpeg decoder are substituted in [`crate::pipeline`] at runtime. The
/// FluxBridge 0xFB [`crate::connection::Connection`] /
/// [`crate::decode_loop::run_decode_loop`] path is NOT used here; it remains for
/// a future FluxBridge-native server.
pub fn stream_video<R: Read>(
    reader: &mut R,
    tunnel_forward: bool,
    decoder: &mut dyn VideoDecoder,
    frame_sink: &Sender<DecodedFrame>,
) -> CoreResult<VideoHeader> {
    if tunnel_forward {
        consume_dummy_byte(reader)
            .map_err(|e| fluxbridge_core::CoreError::Other(format!("scrcpy dummy byte: {e}")))?;
    }

    let header = parse_video_header(reader)
        .map_err(|e| fluxbridge_core::CoreError::Other(format!("scrcpy video header: {e}")))?;

    let config = VideoConfig {
        codec: codec_name(header.codec_id),
        width: header.width,
        height: header.height,
        extra_data: Vec::new(),
    };
    decoder.configure(&config)?;

    stream_av_packets(reader, decoder, frame_sink, &never_cancel)?;
    Ok(header)
}

/// A cancel predicate that never cancels, for callers (the all-in-one
/// [`stream_video`] entry point and tests) that do not drive a stop control.
pub fn never_cancel() -> bool {
    false
}

/// Run the scrcpy access-unit loop against an ALREADY-configured decoder.
///
/// This is the loop body shared by [`stream_video`] (the headless-testable
/// all-in-one entry point) and [`crate::pipeline`] (which parses the header
/// separately so it can size the control sink from the device resolution before
/// frames start flowing). For every access unit:
///
/// * `is_config` units carry SPS/PPS and re-configure the decoder with that
///   extradata before the first frame;
/// * every other unit is decoded and, if the decoder produced drawable pixels
///   (a non-empty buffer), forwarded to `frame_sink`.
///
/// Returns cleanly at end of stream or when the `frame_sink` receiver is gone.
///
/// `should_cancel` is checked once per iteration so a cooperative stop (the
/// "Stop mirroring" control) breaks the loop promptly between access units.
/// Because the underlying socket read can block, the caller sets a read timeout
/// so the loop wakes periodically and observes the flag; a read that times out
/// at a packet boundary surfaces as [`PacketRead::TimedOut`], which the loop
/// treats as a wake (a `continue` that re-checks `should_cancel`) rather than an
/// error, so Stop is observed within one timeout on a stalled stream. A timeout
/// that splits a packet header is still a fatal truncated-packet error.
/// Callers that never cancel pass a closure that always returns `false` (see
/// [`never_cancel`]).
pub fn stream_av_packets<R: Read>(
    reader: &mut R,
    decoder: &mut dyn VideoDecoder,
    frame_sink: &Sender<DecodedFrame>,
    should_cancel: &dyn Fn() -> bool,
) -> CoreResult<()> {
    loop {
        if should_cancel() {
            // A cooperative stop was requested: return cleanly so the pipeline
            // runs its kill-child + teardown path exactly once.
            return Ok(());
        }
        let packet = match read_av_packet(reader) {
            Ok(PacketRead::Packet(p)) => p,
            Ok(PacketRead::Eof) => return Ok(()), // clean end of stream
            // A read timeout at a packet boundary is just a wake: loop back to
            // re-check the cancel flag (above) so Stop is observed promptly on a
            // stalled stream instead of waiting out the whole timeout.
            Ok(PacketRead::TimedOut) => continue,
            Err(e) => {
                return Err(fluxbridge_core::CoreError::Other(format!(
                    "scrcpy access unit: {e}"
                )))
            }
        };

        if packet.is_config {
            // Config access units carry SPS/PPS. Re-configure the decoder with
            // the codec extradata so it can initialize before the first frame.
            // Dimensions are left at 0 here because only the pipeline/header
            // path knows them; a real decoder reads geometry from the stream.
            let cfg = VideoConfig {
                codec: "h264".to_string(),
                width: 0,
                height: 0,
                extra_data: packet.data,
            };
            decoder.configure(&cfg)?;
            continue;
        }

        let frame = decoder.decode(&packet.data)?;
        // The decoder returns an empty buffer when it needs more input before a
        // frame is ready; skip those so the sink only sees drawable frames.
        if frame.data.is_empty() {
            continue;
        }
        if frame_sink.send(frame).is_err() {
            return Ok(()); // UI gone
        }
    }
}

/// Human-readable codec name for a scrcpy fourcc `u32`.
///
/// scrcpy encodes the codec as a four-character code (for example `h264`).
/// Unknown codes fall back to `"h264"` since that is the only codec FluxBridge
/// requests from the server today.
fn codec_name(codec_id: u32) -> String {
    let bytes = codec_id.to_be_bytes();
    match &bytes {
        b"h264" => "h264".to_string(),
        b"h265" => "h265".to_string(),
        b"av1\0" | b"av01" => "av1".to_string(),
        _ => "h264".to_string(),
    }
}

/// Encode a scrcpy `INJECT_KEYCODE` control message.
///
/// Layout: `[type=0][action u8][keycode u32 BE][repeat u32 BE][metastate u32 BE]`.
pub fn encode_inject_keycode(action: u8, keycode: u32, repeat: u32, metastate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(14);
    out.push(CTRL_INJECT_KEYCODE);
    out.push(action);
    out.write_u32::<BigEndian>(keycode).unwrap();
    out.write_u32::<BigEndian>(repeat).unwrap();
    out.write_u32::<BigEndian>(metastate).unwrap();
    out
}

/// Encode a scrcpy `INJECT_TOUCH_EVENT` control message.
///
/// Layout:
/// `[type=2][action u8][pointer_id u64 BE][x i32 BE][y i32 BE]`
/// `[screen_w u16 BE][screen_h u16 BE][pressure u16 BE][action_button u32 BE][buttons u32 BE]`.
#[allow(clippy::too_many_arguments)]
pub fn encode_inject_touch(
    action: u8,
    pointer_id: u64,
    x: i32,
    y: i32,
    screen_w: u16,
    screen_h: u16,
    pressure: u16,
    buttons: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(CTRL_INJECT_TOUCH);
    out.push(action);
    out.write_u64::<BigEndian>(pointer_id).unwrap();
    out.write_i32::<BigEndian>(x).unwrap();
    out.write_i32::<BigEndian>(y).unwrap();
    out.write_u16::<BigEndian>(screen_w).unwrap();
    out.write_u16::<BigEndian>(screen_h).unwrap();
    out.write_u16::<BigEndian>(pressure).unwrap();
    // action_button mirrors buttons for a single-button press in scrcpy >= 2.0.
    out.write_u32::<BigEndian>(buttons).unwrap();
    out.write_u32::<BigEndian>(buttons).unwrap();
    out
}

/// Normalize a FluxBridge pressure value (`0..=65535`) into the `u16` scrcpy
/// expects, where `65535` is full pressure for a press and `0` for a release.
fn press_pressure(down: bool) -> u16 {
    if down {
        0xFFFF
    } else {
        0
    }
}

/// Maps the scrcpy left mouse button bit.
const SCRCPY_BUTTON_PRIMARY: u32 = 1;

/// An [`fluxbridge_core::InputSink`] that writes scrcpy control messages to the
/// control socket.
///
/// This is the bridge that lets the existing FluxBridge input abstraction drive
/// a scrcpy server: FluxBridge [`MouseEvent`] / [`KeyEvent`] / [`TouchEvent`]
/// values are translated into the scrcpy control messages encoded above and
/// written to `W` (the control [`std::net::TcpStream`] in production).
pub struct ScrcpyControlSink<W: Write + Send> {
    writer: W,
    screen_w: u16,
    screen_h: u16,
}

impl<W: Write + Send> ScrcpyControlSink<W> {
    /// Create a sink writing to `writer`; `screen_w`/`screen_h` are the device
    /// resolution scrcpy needs to scale injected pointer coordinates.
    pub fn new(writer: W, screen_w: u16, screen_h: u16) -> Self {
        ScrcpyControlSink {
            writer,
            screen_w,
            screen_h,
        }
    }

    fn write_msg(&mut self, bytes: &[u8]) -> fluxbridge_core::CoreResult<()> {
        self.writer
            .write_all(bytes)
            .and_then(|_| self.writer.flush())
            .map_err(|e| fluxbridge_core::CoreError::Other(format!("scrcpy control write: {e}")))
    }
}

impl<W: Write + Send> fluxbridge_core::InputSink for ScrcpyControlSink<W> {
    fn send_mouse(&mut self, event: MouseEvent) -> fluxbridge_core::CoreResult<()> {
        // Map the FluxBridge mouse action (0 move, 1 down, 2 up, 3 scroll) onto
        // a scrcpy touch event using a fixed pointer id for the mouse.
        let (action, down) = match event.action {
            1 => (ACTION_DOWN, true),
            2 => (ACTION_UP, false),
            _ => (ACTION_MOVE, true),
        };
        let buttons = if down && event.button != 0 {
            SCRCPY_BUTTON_PRIMARY
        } else {
            0
        };
        let msg = encode_inject_touch(
            action,
            u64::MAX, // scrcpy's reserved "mouse" pointer id
            i32::from(event.x),
            i32::from(event.y),
            self.screen_w,
            self.screen_h,
            press_pressure(down),
            buttons,
        );
        self.write_msg(&msg)
    }

    fn send_key(&mut self, event: KeyEvent) -> fluxbridge_core::CoreResult<()> {
        // FluxBridge key action: 0 up, 1 down, 2 repeat.
        let (action, repeat) = match event.action {
            0 => (ACTION_UP, 0),
            2 => (ACTION_DOWN, 1),
            _ => (ACTION_DOWN, 0),
        };
        let msg = encode_inject_keycode(action, event.keycode, repeat, u32::from(event.modifiers));
        self.write_msg(&msg)
    }

    fn send_touch(&mut self, event: TouchEvent) -> fluxbridge_core::CoreResult<()> {
        // FluxBridge touch action: 0 down, 1 up, 2 move, 3 cancel.
        let action = match event.action {
            0 => ACTION_DOWN,
            1 => ACTION_UP,
            _ => ACTION_MOVE,
        };
        let down = event.action == 0 || event.action == 2;
        let msg = encode_inject_touch(
            action,
            u64::from(event.pointer_id),
            i32::from(event.x),
            i32::from(event.y),
            self.screen_w,
            self.screen_h,
            event.pressure,
            if down { SCRCPY_BUTTON_PRIMARY } else { 0 },
        );
        self.write_msg(&msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxbridge_core::InputSink;

    #[test]
    fn inject_keycode_exact_bytes() {
        // action = down(0), keycode = 66 (ENTER), repeat = 0, metastate = 0.
        let bytes = encode_inject_keycode(ACTION_DOWN, 66, 0, 0);
        assert_eq!(
            bytes,
            vec![
                0x00, // type INJECT_KEYCODE
                0x00, // action down
                0x00, 0x00, 0x00, 0x42, // keycode 66
                0x00, 0x00, 0x00, 0x00, // repeat 0
                0x00, 0x00, 0x00, 0x00, // metastate 0
            ]
        );
    }

    #[test]
    fn inject_keycode_with_repeat_and_meta() {
        let bytes = encode_inject_keycode(ACTION_UP, 0x0100_0001, 3, 0x1234_5678);
        assert_eq!(
            bytes,
            vec![
                0x00, // type
                0x01, // action up
                0x01, 0x00, 0x00, 0x01, // keycode
                0x00, 0x00, 0x00, 0x03, // repeat 3
                0x12, 0x34, 0x56, 0x78, // metastate
            ]
        );
    }

    #[test]
    fn inject_touch_tap_exact_bytes() {
        // A primary tap at (100, 200) on a 1080x1920 screen, full pressure.
        let bytes = encode_inject_touch(
            ACTION_DOWN,
            0xFFFF_FFFF_FFFF_FFFF,
            100,
            200,
            1080,
            1920,
            0xFFFF,
            SCRCPY_BUTTON_PRIMARY,
        );
        assert_eq!(
            bytes,
            vec![
                0x02, // type INJECT_TOUCH_EVENT
                0x00, // action down
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // pointer id u64
                0x00, 0x00, 0x00, 0x64, // x = 100
                0x00, 0x00, 0x00, 0xC8, // y = 200
                0x04, 0x38, // screen width 1080
                0x07, 0x80, // screen height 1920
                0xFF, 0xFF, // pressure
                0x00, 0x00, 0x00, 0x01, // action button
                0x00, 0x00, 0x00, 0x01, // buttons
            ]
        );
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn mouse_event_maps_to_touch_message() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut sink = ScrcpyControlSink::new(&mut buf, 1080, 1920);
            sink.send_mouse(MouseEvent {
                x: 100,
                y: 200,
                button: 1,
                action: 1, // down
                scroll: 0,
            })
            .unwrap();
        }
        // type byte is INJECT_TOUCH_EVENT, action down, uses the mouse pointer id.
        assert_eq!(buf[0], CTRL_INJECT_TOUCH);
        assert_eq!(buf[1], ACTION_DOWN);
        assert_eq!(&buf[2..10], &[0xFF; 8]); // u64::MAX pointer id
        assert_eq!(&buf[10..14], &[0x00, 0x00, 0x00, 0x64]); // x 100
        assert_eq!(&buf[14..18], &[0x00, 0x00, 0x00, 0xC8]); // y 200
        assert_eq!(buf.len(), 32);
    }

    #[test]
    fn key_event_maps_to_keycode_message() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut sink = ScrcpyControlSink::new(&mut buf, 1080, 1920);
            sink.send_key(KeyEvent {
                keycode: 66,
                scancode: 0,
                action: 1, // down
                modifiers: 0,
            })
            .unwrap();
        }
        assert_eq!(buf, encode_inject_keycode(ACTION_DOWN, 66, 0, 0));
    }

    #[test]
    fn touch_event_maps_to_touch_message() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut sink = ScrcpyControlSink::new(&mut buf, 720, 1280);
            sink.send_touch(TouchEvent {
                pointer_id: 1,
                x: 10,
                y: 20,
                action: 0, // down
                pressure: 0xFFFF,
            })
            .unwrap();
        }
        let expected = encode_inject_touch(
            ACTION_DOWN,
            1,
            10,
            20,
            720,
            1280,
            0xFFFF,
            SCRCPY_BUTTON_PRIMARY,
        );
        assert_eq!(buf, expected);
    }

    #[test]
    fn parse_video_header_reads_name_codec_and_size() {
        let mut stream: Vec<u8> = Vec::new();
        let mut name = [0u8; DEVICE_NAME_LEN];
        name[..6].copy_from_slice(b"Pixel7");
        stream.extend_from_slice(&name);
        stream.extend_from_slice(&fourcc(b"h264").to_be_bytes());
        stream.extend_from_slice(&1080u32.to_be_bytes());
        stream.extend_from_slice(&2400u32.to_be_bytes());

        let mut cursor = std::io::Cursor::new(stream);
        let header = parse_video_header(&mut cursor).unwrap();
        assert_eq!(header.device_name, "Pixel7");
        assert_eq!(header.codec_id, fourcc(b"h264"));
        assert_eq!(header.width, 1080);
        assert_eq!(header.height, 2400);
    }

    #[test]
    fn read_av_packet_frames_access_units_and_flags() {
        let mut stream: Vec<u8> = Vec::new();
        // Config packet: config flag set, pts 0, 3 bytes.
        let pts0 = PACKET_FLAG_CONFIG;
        stream.extend_from_slice(&pts0.to_be_bytes());
        stream.extend_from_slice(&3u32.to_be_bytes());
        stream.extend_from_slice(&[0x67, 0x42, 0x00]);
        // Keyframe: key flag set, pts 1000, 4 bytes.
        let pts1 = PACKET_FLAG_KEY_FRAME | 1000;
        stream.extend_from_slice(&pts1.to_be_bytes());
        stream.extend_from_slice(&4u32.to_be_bytes());
        stream.extend_from_slice(&[0x65, 0x88, 0x84, 0x00]);

        let mut cursor = std::io::Cursor::new(stream);

        let p0 = match read_av_packet(&mut cursor).unwrap() {
            PacketRead::Packet(p) => p,
            other => panic!("expected a packet, got {other:?}"),
        };
        assert!(p0.is_config);
        assert!(!p0.is_keyframe);
        assert_eq!(p0.pts_us, 0);
        assert_eq!(p0.data, vec![0x67, 0x42, 0x00]);

        let p1 = match read_av_packet(&mut cursor).unwrap() {
            PacketRead::Packet(p) => p,
            other => panic!("expected a packet, got {other:?}"),
        };
        assert!(!p1.is_config);
        assert!(p1.is_keyframe);
        assert_eq!(p1.pts_us, 1000);
        assert_eq!(p1.data, vec![0x65, 0x88, 0x84, 0x00]);

        // Clean end of stream.
        assert!(
            matches!(read_av_packet(&mut cursor).unwrap(), PacketRead::Eof),
            "an exhausted cursor must read as a clean Eof"
        );
    }

    /// A read timeout at a packet boundary (no bytes of the next packet
    /// consumed) surfaces as [`PacketRead::TimedOut`], not an error or Eof, so
    /// the stream loop can wake, re-check its cancel flag, and retry. This is
    /// what bounds Stop latency on a stalled stream. A reader whose first read
    /// returns `WouldBlock`/`TimedOut` stands in for the live socket's read
    /// timeout firing with no data available.
    #[test]
    fn read_av_packet_reports_timeout_at_boundary_as_wake() {
        struct TimingOutReader;
        impl Read for TimingOutReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "read timed out"))
            }
        }
        let mut r = TimingOutReader;
        assert!(
            matches!(read_av_packet(&mut r).unwrap(), PacketRead::TimedOut),
            "a boundary timeout must be a wake, not an error or Eof"
        );
    }

    /// A read timeout that fires AFTER some but not all 8 pts-header bytes are
    /// consumed splits the header. Partial framing cannot be resumed (retrying
    /// would re-read from a desynced position), so this must surface as a
    /// truncated-packet ERROR, NOT [`PacketRead::TimedOut`]. This guards the
    /// boundary-vs-mid-packet contract the Stop-latency fix depends on.
    #[test]
    fn read_av_packet_errors_on_split_header_timeout() {
        // First read yields 4 of the 8 header bytes, then the read times out.
        struct SplitHeaderReader {
            delivered: usize,
        }
        impl Read for SplitHeaderReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.delivered == 0 {
                    let n = 4.min(buf.len());
                    for b in buf[..n].iter_mut() {
                        *b = 0;
                    }
                    self.delivered += n;
                    Ok(n)
                } else {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "read timed out"))
                }
            }
        }
        let mut r = SplitHeaderReader { delivered: 0 };
        let err = read_av_packet(&mut r)
            .expect_err("a mid-header timeout must be an error, not a TimedOut wake");
        assert_eq!(
            err.kind(),
            io::ErrorKind::UnexpectedEof,
            "a split pts-header timeout is a truncated-packet error"
        );
    }

    #[test]
    fn read_av_packet_errors_on_truncated_header() {
        // Only 3 bytes where an 8-byte pts header is expected.
        let mut cursor = std::io::Cursor::new(vec![0u8, 0u8, 0u8]);
        let err = read_av_packet(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A fake decoder that records the config it saw and the access units it
    /// decoded, and tags each decoded frame as drawable `"rgba"` so the stream
    /// driver forwards it.
    #[derive(Default)]
    struct FakeDecoder {
        configured_with: Vec<VideoConfig>,
        decoded: Vec<Vec<u8>>,
    }

    impl VideoDecoder for FakeDecoder {
        fn configure(&mut self, config: &VideoConfig) -> CoreResult<()> {
            self.configured_with.push(config.clone());
            Ok(())
        }

        fn decode(&mut self, encoded: &[u8]) -> CoreResult<DecodedFrame> {
            self.decoded.push(encoded.to_vec());
            Ok(DecodedFrame {
                width: 2,
                height: 2,
                pixel_format: "rgba".to_string(),
                data: encoded.to_vec(),
            })
        }

        fn flush(&mut self) -> CoreResult<()> {
            Ok(())
        }
    }

    /// Build a synthetic scrcpy video stream: [optional dummy byte][header]
    /// [config access unit][frame access unit]...
    fn synthetic_stream(with_dummy: bool, frames: &[(bool, &[u8])]) -> Vec<u8> {
        let mut stream: Vec<u8> = Vec::new();
        if with_dummy {
            stream.push(0x00);
        }
        let mut name = [0u8; DEVICE_NAME_LEN];
        name[..6].copy_from_slice(b"Pixel7");
        stream.extend_from_slice(&name);
        stream.extend_from_slice(&fourcc(b"h264").to_be_bytes());
        stream.extend_from_slice(&1080u32.to_be_bytes());
        stream.extend_from_slice(&2400u32.to_be_bytes());
        for (is_config, data) in frames {
            let pts = if *is_config { PACKET_FLAG_CONFIG } else { 0 };
            stream.extend_from_slice(&pts.to_be_bytes());
            stream.extend_from_slice(&(data.len() as u32).to_be_bytes());
            stream.extend_from_slice(data);
        }
        stream
    }

    #[test]
    fn consume_dummy_byte_reads_exactly_one() {
        let mut cursor = std::io::Cursor::new(vec![0x00u8, 0xAA, 0xBB]);
        consume_dummy_byte(&mut cursor).unwrap();
        // The next read should see 0xAA, confirming only one byte was consumed.
        let mut rest = [0u8; 2];
        cursor.read_exact(&mut rest).unwrap();
        assert_eq!(rest, [0xAA, 0xBB]);
    }

    #[test]
    fn stream_video_drives_decoder_from_header_and_access_units() {
        // tunnel-forward stream: dummy byte, header, one config + two frames.
        let stream = synthetic_stream(
            true,
            &[
                (true, &[0x67, 0x42, 0x00][..]),  // config (SPS/PPS)
                (false, &[0x65, 0x88][..]),        // keyframe-ish frame
                (false, &[0x41, 0x9A][..]),        // delta frame
            ],
        );
        let mut cursor = std::io::Cursor::new(stream);
        let mut decoder = FakeDecoder::default();
        let (tx, rx) = std::sync::mpsc::channel();

        let header = stream_video(&mut cursor, true, &mut decoder, &tx).unwrap();
        drop(tx);

        assert_eq!(header.device_name, "Pixel7");
        assert_eq!(header.width, 1080);
        assert_eq!(header.height, 2400);

        // The decoder was configured first from the header (dimensions, no
        // extradata), then again from the config access unit (with the SPS/PPS
        // extradata).
        assert_eq!(decoder.configured_with.len(), 2);
        assert_eq!(decoder.configured_with[0].width, 1080);
        assert_eq!(decoder.configured_with[0].height, 2400);
        assert!(decoder.configured_with[0].extra_data.is_empty());
        assert_eq!(decoder.configured_with[1].extra_data, vec![0x67, 0x42, 0x00]);

        // Only the two non-config access units were decoded.
        assert_eq!(decoder.decoded, vec![vec![0x65, 0x88], vec![0x41, 0x9A]]);

        // Both decoded frames reached the sink in order.
        let frames: Vec<DecodedFrame> = rx.try_iter().collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, vec![0x65, 0x88]);
        assert_eq!(frames[1].data, vec![0x41, 0x9A]);
    }

    #[test]
    fn stream_video_without_tunnel_forward_has_no_dummy_byte() {
        // reverse-tunnel stream: NO dummy byte, header, one frame.
        let stream = synthetic_stream(false, &[(false, &[0x01, 0x02, 0x03][..])]);
        let mut cursor = std::io::Cursor::new(stream);
        let mut decoder = FakeDecoder::default();
        let (tx, rx) = std::sync::mpsc::channel();

        let header = stream_video(&mut cursor, false, &mut decoder, &tx).unwrap();
        drop(tx);

        assert_eq!(header.width, 1080);
        assert_eq!(decoder.decoded, vec![vec![0x01, 0x02, 0x03]]);
        let frames: Vec<DecodedFrame> = rx.try_iter().collect();
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn stream_video_skips_empty_decoder_output() {
        // A decoder that always needs more input emits empty frames; none of
        // them should reach the sink.
        #[derive(Default)]
        struct NeedsMoreInput;
        impl VideoDecoder for NeedsMoreInput {
            fn configure(&mut self, _c: &VideoConfig) -> CoreResult<()> {
                Ok(())
            }
            fn decode(&mut self, _e: &[u8]) -> CoreResult<DecodedFrame> {
                Ok(DecodedFrame {
                    width: 1080,
                    height: 2400,
                    pixel_format: "rgba".to_string(),
                    data: Vec::new(), // needs more input
                })
            }
            fn flush(&mut self) -> CoreResult<()> {
                Ok(())
            }
        }

        let stream = synthetic_stream(false, &[(false, &[0xAA][..]), (false, &[0xBB][..])]);
        let mut cursor = std::io::Cursor::new(stream);
        let mut decoder = NeedsMoreInput;
        let (tx, rx) = std::sync::mpsc::channel();

        stream_video(&mut cursor, false, &mut decoder, &tx).unwrap();
        drop(tx);

        let frames: Vec<DecodedFrame> = rx.try_iter().collect();
        assert!(frames.is_empty(), "empty decoder output must not be forwarded");
    }

    #[test]
    fn stream_video_errors_on_truncated_header() {
        // Too few bytes for the 64-byte device name header.
        let mut cursor = std::io::Cursor::new(vec![0u8; 10]);
        let mut decoder = FakeDecoder::default();
        let (tx, _rx) = std::sync::mpsc::channel();
        let err = stream_video(&mut cursor, false, &mut decoder, &tx).unwrap_err();
        assert!(err.to_string().contains("scrcpy video header"));
    }

    #[test]
    fn codec_name_maps_known_fourccs() {
        assert_eq!(codec_name(fourcc(b"h264")), "h264");
        assert_eq!(codec_name(fourcc(b"h265")), "h265");
        // Unknown falls back to h264.
        assert_eq!(codec_name(fourcc(b"zzzz")), "h264");
    }
}
