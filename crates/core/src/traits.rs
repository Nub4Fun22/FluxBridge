//! Decoupling trait abstractions.
//!
//! The FluxBridge core never links to platform or vendor libraries directly.
//! Every OS-specific or device-specific capability is expressed as a trait here
//! and implemented behind `#[cfg(...)]`-gated stubs in [`crate::platform`] (and,
//! in production, by real backends that live outside the core crate). This is
//! what keeps the protocol and application logic portable between Windows and
//! Linux and testable on a headless CI host.

use crate::error::CoreResult;
use crate::protocol::{KeyEvent, MouseEvent, TouchEvent};

/// Configuration describing an encoded video stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoConfig {
    /// FourCC / codec identifier, for example `"h264"` or `"hevc"`.
    pub codec: String,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Codec-specific setup bytes (SPS/PPS, etc.).
    pub extra_data: Vec<u8>,
}

/// A single encoded video frame handed off by a [`VideoCapture`] source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Presentation timestamp in milliseconds.
    pub pts_ms: u64,
    /// Whether this frame is a keyframe (IDR).
    pub keyframe: bool,
    /// Encoded bytes.
    pub data: Vec<u8>,
}

/// A decoded (raw) video frame produced by a [`VideoDecoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Pixel format identifier, for example `"nv12"` or `"rgba"`.
    pub pixel_format: String,
    /// Raw pixel bytes (layout depends on `pixel_format`).
    pub data: Vec<u8>,
}

/// Source of an encoded video stream from a connected device.
///
/// Real implementations drive the Android `MediaCodec`/`SurfaceControl` capture
/// server over ADB, or an iOS AirPlay / RTP H.264 receiver. The core only sees
/// [`EncodedFrame`] values.
pub trait VideoCapture: Send {
    /// Begin capturing. Returns the negotiated stream configuration.
    fn start(&mut self) -> CoreResult<VideoConfig>;

    /// Pull the next encoded frame, if one is available.
    fn next_frame(&mut self) -> CoreResult<Option<EncodedFrame>>;

    /// Stop capturing and release resources.
    fn stop(&mut self) -> CoreResult<()>;
}

/// Decoder that turns [`EncodedFrame`] data into [`DecodedFrame`] pixels.
///
/// Production backends use FFmpeg with hardware acceleration: DXVA2 / D3D11VA on
/// Windows and VA-API / VDPAU on Linux. The core treats the decoder as an opaque
/// trait object so that no FFmpeg symbols leak into the portable crate.
pub trait VideoDecoder: Send {
    /// Configure the decoder for a given stream.
    fn configure(&mut self, config: &VideoConfig) -> CoreResult<()>;

    /// Decode a single encoded frame into raw pixels.
    fn decode(&mut self, encoded: &[u8]) -> CoreResult<DecodedFrame>;

    /// Decode exactly one access unit, carrying the stream framing flags.
    ///
    /// This is the seam the scrcpy stream driver uses so a decoder learns which
    /// access units are config (SPS/PPS) units and which are keyframes, WITHOUT
    /// the driver having to decide how a config unit is handled. That decision
    /// belongs to the decoder, because different backends need it handled
    /// differently:
    ///
    /// * The native engine (scrcpy's proven algorithm) must PREPEND a config
    ///   unit to the next media packet and send the two as one Annex-B packet.
    ///   Delivering the config unit through a separate `configure()` call with
    ///   empty dimensions and a standalone extradata buffer is the root cause of
    ///   the `send_packet: invalid data found when processing input` error, so
    ///   that backend overrides this method to feed config bytes straight into
    ///   its internal packet merger.
    /// * The pure-Rust stub and the `ffmpeg-next` fallback keep their existing
    ///   behaviour via the default implementation below.
    ///
    /// The default implementation preserves the pre-existing contract: a config
    /// unit is forwarded to [`VideoDecoder::configure`] as `extra_data` (with
    /// zero dimensions, which non-native decoders already tolerate) and yields
    /// an empty [`DecodedFrame`] (no pixels are produced from a config unit),
    /// while a media unit is forwarded to [`VideoDecoder::decode`]. A decoder
    /// that needs the config bytes merged into the next media packet overrides
    /// this method instead of relying on the default.
    ///
    /// Returning an empty-`data` [`DecodedFrame`] signals "no drawable frame
    /// yet" to the caller, which only forwards non-empty frames to the UI.
    fn decode_unit(
        &mut self,
        encoded: &[u8],
        is_config: bool,
        _is_keyframe: bool,
    ) -> CoreResult<DecodedFrame> {
        if is_config {
            // Preserve the historical default: deliver the config unit as
            // decoder extra_data. Dimensions are unknown at this seam (only the
            // header/pipeline path knows them), so they are left at 0, which the
            // stub and ffmpeg-next decoders already tolerate (they keep the
            // non-zero geometry learned earlier). No drawable frame is produced
            // from a config unit, so an empty frame is returned.
            let cfg = VideoConfig {
                codec: "h264".to_string(),
                width: 0,
                height: 0,
                extra_data: encoded.to_vec(),
            };
            self.configure(&cfg)?;
            Ok(DecodedFrame {
                width: 0,
                height: 0,
                pixel_format: "rgba".to_string(),
                data: Vec::new(),
            })
        } else {
            self.decode(encoded)
        }
    }

    /// Flush any buffered frames and reset decoder state.
    fn flush(&mut self) -> CoreResult<()>;
}

/// Sink that delivers input events to the connected device.
///
/// On Android this maps to `InputManager.injectInputEvent` over the control
/// socket; on iOS to WebDriverAgent actions.
pub trait InputSink: Send {
    /// Forward a mouse/pointer event.
    fn send_mouse(&mut self, event: MouseEvent) -> CoreResult<()>;

    /// Forward a keyboard event.
    fn send_key(&mut self, event: KeyEvent) -> CoreResult<()>;

    /// Forward a touch event.
    fn send_touch(&mut self, event: TouchEvent) -> CoreResult<()>;
}

/// A file offered to the host during a reverse (phone to PC) drag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragFile {
    /// Suggested file name.
    pub name: String,
    /// MIME type.
    pub mime: String,
    /// Total size in bytes, if known.
    pub size: u64,
}

/// Bridge for bi-directional drag-and-drop between the host desktop and device.
///
/// On Windows the host side is implemented with OLE `IDataObject` /
/// `IDropSource`; on Linux with XDnD (X11) or the `wl_data_device` protocol
/// (Wayland). The core only coordinates the hand-off.
pub trait DragDropBridge: Send {
    /// Begin a reverse drag (phone to PC): the device is offering `files` and
    /// the host should start a native drag operation.
    fn begin_reverse_drag(&mut self, files: &[DragFile]) -> CoreResult<()>;

    /// Offer a single file to the host's current drop target, returning the
    /// local path it was materialized to, if the drop completed.
    fn offer_file(&mut self, file: &DragFile, bytes: &[u8]) -> CoreResult<Option<String>>;
}

/// An image on the clipboard, in a portable raw form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Pixel format identifier, for example `"rgba"`.
    pub pixel_format: String,
    /// Raw pixel bytes.
    pub data: Vec<u8>,
}

/// Bi-directional clipboard synchronization (text and images).
pub trait ClipboardSync: Send {
    /// Push host clipboard text to the device.
    fn push_text(&mut self, text: &str) -> CoreResult<()>;

    /// Pull the device clipboard text, if any.
    fn pull_text(&mut self) -> CoreResult<Option<String>>;

    /// Push a host clipboard image to the device.
    fn push_image(&mut self, image: &ClipboardImage) -> CoreResult<()>;

    /// Pull a device clipboard image, if any.
    fn pull_image(&mut self) -> CoreResult<Option<ClipboardImage>>;
}

/// Information about an available release discovered by an [`Updater`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// Version tag, for example `"v1.2.0"`.
    pub version: String,
    /// Human-readable release notes / changelog.
    pub notes: String,
    /// Download URL for the platform asset.
    pub asset_url: String,
}

/// In-app updater abstraction.
///
/// The concrete GitHub Releases implementation lives in the `fluxbridge-updater`
/// crate (added in FEAT-002). The signature is declared here so that core
/// consumers can depend on the abstraction.
pub trait Updater: Send {
    /// Check for a newer release than `current_version`.
    fn check(&self, current_version: &str) -> CoreResult<Option<ReleaseInfo>>;

    /// Download the release asset, returning the local path of the downloaded
    /// installer / binary.
    fn download(&self, release: &ReleaseInfo) -> CoreResult<String>;

    /// Apply a previously downloaded update located at `downloaded_path`.
    fn apply(&self, downloaded_path: &str) -> CoreResult<()>;
}
