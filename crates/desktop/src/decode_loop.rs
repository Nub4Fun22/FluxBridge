//! Decoders and the FluxBridge-native (`0xFB`) decode loop.
//!
//! This module owns the [`VideoDecoder`] implementations ([`StubDecoder`] and,
//! behind the `decode` feature, [`FfmpegDecoder`]) plus [`run_decode_loop`],
//! which pulls [`PacketType::VideoFrame`] packets off a FluxBridge
//! [`crate::connection::Connection`] channel and feeds them to a decoder.
//!
//! IMPORTANT: [`run_decode_loop`] and the `0xFB` [`crate::connection::Connection`]
//! it consumes from are the FluxBridge-NATIVE path, retained for a future
//! FluxBridge-native capture server. They are NOT on the shipped v1 scrcpy
//! mirroring path. The v1 path drives the scrcpy wire format directly (see
//! [`crate::scrcpy::stream_av_packets`], called from [`crate::pipeline`]), which
//! feeds these same decoders without going through `run_decode_loop`. The
//! decoder trait object is the shared seam between both paths.
//!
//! The decoder is a trait object so the real backend (FFmpeg with DXVA2 /
//! D3D11VA on Windows, VA-API / VDPAU on Linux) can be dropped in without
//! touching the loop. [`StubDecoder`] is a pure-Rust placeholder that echoes the
//! encoded bytes so the default and `gui` builds run with no FFmpeg linkage; it
//! is NOT a real codec. The real pixels come from [`FfmpegDecoder`] under the
//! `decode` feature.

use std::sync::mpsc::{Receiver, Sender};

use fluxbridge_core::protocol::{Packet, PacketType};
use fluxbridge_core::{CoreResult, DecodedFrame, VideoConfig, VideoDecoder};

/// Copy a `width x height` RGBA image out of a buffer whose rows are
/// `src_stride` bytes apart into a tightly packed `width*height*4` buffer.
///
/// `swscale` (and most image pipelines) align each output row to a stride that
/// can be LARGER than the visible `width*4` bytes, leaving padding at the end of
/// every row. The UI only draws a [`DecodedFrame`] whose
/// `data.len() == width*height*4` (see
/// [`crate::ui`]::`decoded_frame_to_image`), so the decoder must drop that
/// padding before handing a frame off. This helper does exactly that: it reads
/// `width*4` bytes from the start of each of the `height` rows and concatenates
/// them, producing a contiguous buffer with no row padding.
///
/// It is intentionally a pure, NON-FFmpeg function (not behind the `decode`
/// feature) so the stride math can be unit-tested on the headless Linux host
/// without linking libav.
///
/// # Panics
///
/// Panics (via slice bounds) if `src` is not large enough to hold `height` rows
/// of `src_stride` bytes, or if `src_stride < width*4`. Callers pass libav's own
/// reported stride and frame geometry, which always satisfy these invariants.
pub(crate) fn pack_rgba(src: &[u8], src_stride: usize, width: usize, height: usize) -> Vec<u8> {
    let row_bytes = width * 4;
    debug_assert!(
        src_stride >= row_bytes,
        "src_stride must be at least width*4"
    );
    let mut out = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        let start = row * src_stride;
        out.extend_from_slice(&src[start..start + row_bytes]);
    }
    out
}

/// A stand-in software decoder.
///
/// It does not actually decode H.264/HEVC; it wraps each encoded payload in a
/// [`DecodedFrame`] sized from its configuration so the pipeline (connection ->
/// decode -> sink -> UI) can be exercised without a hardware codec. The real
/// FFmpeg-backed `VideoDecoder` replaces this; see `docs/ARCHITECTURE.md`.
#[derive(Debug, Default)]
pub struct StubDecoder {
    width: u32,
    height: u32,
    configured: bool,
}

impl StubDecoder {
    /// Create an unconfigured stub decoder.
    pub fn new() -> Self {
        StubDecoder {
            width: 0,
            height: 0,
            configured: false,
        }
    }
}

impl VideoDecoder for StubDecoder {
    fn configure(&mut self, config: &VideoConfig) -> CoreResult<()> {
        self.width = config.width;
        self.height = config.height;
        self.configured = true;
        Ok(())
    }

    fn decode(&mut self, encoded: &[u8]) -> CoreResult<DecodedFrame> {
        // Produce a deterministic placeholder frame. Real decoders return the
        // actual decoded pixels; here we echo the input length into a tiny
        // buffer so tests can assert data flowed through.
        Ok(DecodedFrame {
            width: self.width,
            height: self.height,
            pixel_format: "stub".to_string(),
            data: encoded.to_vec(),
        })
    }

    fn flush(&mut self) -> CoreResult<()> {
        Ok(())
    }
}

/// Real FFmpeg-backed H.264 decoder, compiled only under the `decode` feature.
///
/// This is the shipping decode path. It is gated behind `decode` (OFF by
/// default) because it links FFmpeg/libav via the `ffmpeg-next` crate, which
/// needs the libav development libraries present at build/link time. Those are
/// NOT installed on the headless Linux CI host, so the default and `gui` builds
/// use [`StubDecoder`] instead and never pull FFmpeg in.
///
/// # Windows build/link requirements (honest notes)
///
/// `ffmpeg-next` binds the system FFmpeg. On Windows the practical path is to
/// install the FFmpeg "dev" (shared + headers) build, typically via `vcpkg`
/// (`vcpkg install ffmpeg`) and point `FFMPEG_DIR` / `PKG_CONFIG_PATH` at it so
/// the `ffmpeg-sys-next` build script can find `avcodec`, `avformat`,
/// `avutil`, and `swscale`. The CI release workflow provisions these before a
/// `--features decode` build. Hardware acceleration on Windows is DXVA2 or
/// D3D11VA; this software path is the portable baseline and a hwaccel device
/// can be attached to the same `decoder::Video` later without changing the
/// [`VideoDecoder`] trait boundary. On Linux the equivalent dev packages are
/// `libavcodec-dev` / `libavutil-dev` / `libswscale-dev` and the hwaccel is
/// VA-API / VDPAU.
///
/// The decoder feeds each scrcpy access unit (an Annex-B H.264 NAL run) as a
/// `Packet` to the libav H.264 decoder and converts the first decoded frame to
/// a tightly packed RGBA buffer with `swscale`, which is directly usable as a
/// `slint::Image` (`SharedPixelBuffer<Rgba8Pixel>`).
///
/// A `Send` newtype around the swscale scaling context.
///
/// `ffmpeg_next::software::scaling::Context` wraps a raw `*mut SwsContext`,
/// which is not `Send`, so holding one directly inside [`FfmpegDecoder`] would
/// make the whole decoder non-`Send` and break the `VideoDecoder: Send` bound
/// the pipeline relies on (the decoder is moved onto a dedicated decode worker
/// thread; see `crate::pipeline`).
///
/// # Safety
///
/// The `unsafe impl Send` below is sound HERE because of how the scaler is
/// used, NOT because an swscale context is generally thread-safe:
///
/// * [`FfmpegDecoder`] owns its `SendScaler` exclusively (a private
///   `Option<SendScaler>` field); it is never shared, cloned, aliased, or
///   wrapped in an `Arc`/reference handed to another thread.
/// * The scaler is only ever touched from the single decode worker thread
///   inside [`FfmpegDecoder::decode`]; there is no concurrent access from two
///   threads at any time.
/// * The only cross-thread action is MOVING the owning `FfmpegDecoder` (and
///   with it the pointer) onto the worker thread once, before any use. libav's
///   libswscale contexts are not safe for CONCURRENT use from multiple threads,
///   but an exclusive move followed by single-threaded use is fine: the raw
///   `*mut SwsContext` is just an owned heap handle with no thread affinity.
#[cfg(feature = "decode")]
struct SendScaler(ffmpeg_next::software::scaling::Context);

// SAFETY: see the `SendScaler` doc comment above. The scaler is owned
// exclusively by one `FfmpegDecoder`, used only from the single decode worker
// thread, and merely moved (never shared) across the thread boundary.
#[cfg(feature = "decode")]
unsafe impl Send for SendScaler {}

#[cfg(feature = "decode")]
pub struct FfmpegDecoder {
    decoder: Option<ffmpeg_next::decoder::Video>,
    scaler: Option<SendScaler>,
    /// The (width, height, pixel format) the cached `scaler` was built for.
    ///
    /// swscale contexts are created for a fixed source geometry and pixel
    /// format. A phone mirror can change geometry mid-stream (device rotation,
    /// or the Z Fold5 folding/unfolding), after which libav emits frames at the
    /// NEW dimensions. If we kept a scaler built for the old geometry, its RGBA
    /// output (and `pack_rgba`'s width*height*4 result) would no longer match
    /// the frame the UI expects and every post-change frame would be dropped by
    /// the UI length gate (black/frozen video). We compare each decoded frame's
    /// geometry to this and rebuild the scaler when it differs. The H264
    /// decoder itself is NOT rebuilt; only the scaler follows the frame.
    scaler_src: Option<(u32, u32, ffmpeg_next::format::Pixel)>,
    width: u32,
    height: u32,
}

#[cfg(feature = "decode")]
impl FfmpegDecoder {
    /// Create an unconfigured FFmpeg decoder, initializing libav once.
    pub fn new() -> CoreResult<Self> {
        ffmpeg_next::init()
            .map_err(|e| fluxbridge_core::CoreError::Other(format!("ffmpeg init: {e}")))?;
        Ok(FfmpegDecoder {
            decoder: None,
            scaler: None,
            scaler_src: None,
            width: 0,
            height: 0,
        })
    }
}

#[cfg(feature = "decode")]
impl VideoDecoder for FfmpegDecoder {
    fn configure(&mut self, config: &VideoConfig) -> CoreResult<()> {
        use ffmpeg_next::codec::{context::Context, Id};

        // stream_av_packets calls configure() twice with different intents:
        //   1. ONCE up front from the parsed scrcpy header with the real device
        //      dimensions set and empty extra_data; and
        //   2. AGAIN for the is_config (SPS/PPS) access unit with width=0,
        //      height=0 and extra_data=SPS/PPS. That same config access unit is
        //      ALSO fed through decode() so the SPS/PPS NALs reach send_packet
        //      and prime the h264 parser (see the decode() note below).
        // Only remember non-zero dimensions so a later zero-dim SPS/PPS re-config
        // does not clobber the known-good header geometry. The decoded frame's
        // real geometry always comes from decoded.width()/height() in decode(),
        // so these fields are only the fallback reported on empty frames.
        if config.width != 0 && config.height != 0 {
            self.width = config.width;
            self.height = config.height;
        }

        // If a working decoder already exists, do NOT tear it down on a
        // re-configure. scrcpy sends raw Annex-B H.264 and its codec config
        // (SPS/PPS) arrives ONCE up front as the leading config access unit,
        // which stream_av_packets feeds through decode() to prime the libav
        // h264 parser; rebuilding the decoder (and dropping the scaler)
        // mid-stream would needlessly discard warmed-up state. Build the decoder
        // lazily only the first time.
        if self.decoder.is_some() {
            return Ok(());
        }

        let codec = ffmpeg_next::decoder::find(Id::H264)
            .ok_or_else(|| fluxbridge_core::CoreError::Other("no H264 decoder".into()))?;
        let ctx = Context::new_with_codec(codec);
        let decoder = ctx
            .decoder()
            .video()
            .map_err(|e| fluxbridge_core::CoreError::Other(format!("open h264 decoder: {e}")))?;
        self.decoder = Some(decoder);
        self.scaler = None;
        self.scaler_src = None;
        Ok(())
    }

    fn decode(&mut self, encoded: &[u8]) -> CoreResult<DecodedFrame> {
        use ffmpeg_next::format::Pixel;
        use ffmpeg_next::software::scaling::{Context as Scaler, Flags};
        use ffmpeg_next::util::frame::Video as VideoFrame;

        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| fluxbridge_core::CoreError::Other("decoder not configured".into()))?;

        // scrcpy sends raw Annex-B H.264 NAL runs (one access unit per call).
        // Feeding each access unit straight to the libav H264 decoder via
        // Packet::copy is correct for Annex-B input. scrcpy's codec config
        // (SPS/PPS) is sent ONCE up front as the leading config access unit and
        // is NOT re-inlined before each keyframe, so stream_av_packets feeds
        // that config access unit through this same decode() path first: the
        // SPS/PPS NALs go through send_packet and prime the h264 parser. A pure
        // SPS/PPS access unit produces no frame, so receive_frame returns EAGAIN
        // and we return the empty frame below (no error). Every later frame then
        // decodes against the primed context.
        let packet = ffmpeg_next::Packet::copy(encoded);
        decoder
            .send_packet(&packet)
            .map_err(|e| fluxbridge_core::CoreError::Other(format!("send_packet: {e}")))?;

        let mut decoded = VideoFrame::empty();
        if decoder.receive_frame(&mut decoded).is_err() {
            // No frame ready yet (needs more input); return an empty frame the
            // loop can ignore.
            return Ok(DecodedFrame {
                width: self.width,
                height: self.height,
                pixel_format: "rgba".to_string(),
                data: Vec::new(),
            });
        }

        let w = decoded.width();
        let h = decoded.height();
        let src_format = decoded.format();

        // (Re)build the swscale context whenever the decoded frame geometry or
        // pixel format differs from what the cached scaler was built for. This
        // is the mid-stream resolution-change path: a device rotation (or the
        // Z Fold5 folding/unfolding) makes libav emit frames at a new w/h, and
        // a scaler sized for the old geometry would produce an RGBA buffer
        // whose pack_rgba length no longer equals the new width*height*4, so
        // the UI length gate would silently drop every post-rotation frame
        // (black/frozen video). Only the scaler is rebuilt here; the H264
        // decoder is deliberately reused across re-configures.
        if self.scaler_src != Some((w, h, src_format)) {
            let ctx =
                Scaler::get(src_format, w, h, Pixel::RGBA, w, h, Flags::BILINEAR).map_err(|e| {
                    fluxbridge_core::CoreError::Other(format!("create swscale context: {e}"))
                })?;
            self.scaler = Some(SendScaler(ctx));
            self.scaler_src = Some((w, h, src_format));
        }
        // Unwrap the `SendScaler` newtype to reach the underlying swscale
        // context; see the `SendScaler` safety note for why wrapping is sound.
        let scaler = &mut self
            .scaler
            .as_mut()
            .expect("scaler was just created above")
            .0;
        let mut rgba = VideoFrame::empty();
        scaler
            .run(&decoded, &mut rgba)
            .map_err(|e| fluxbridge_core::CoreError::Other(format!("swscale: {e}")))?;

        // Copy tightly packed RGBA (strip any row stride padding) so the UI's
        // data.len() == width*height*4 gate is satisfied. The row-packing math
        // lives in the pure pack_rgba helper so it can be tested headless.
        let data = pack_rgba(rgba.data(0), rgba.stride(0), w as usize, h as usize);
        Ok(DecodedFrame {
            width: w,
            height: h,
            pixel_format: "rgba".to_string(),
            data,
        })
    }

    fn flush(&mut self) -> CoreResult<()> {
        if let Some(decoder) = self.decoder.as_mut() {
            let _ = decoder.flush();
        }
        Ok(())
    }
}

/// A `Send` newtype owning the raw `fb_decoder` handle.
///
/// The raw `*mut scrcpy_decode_sys::fb_decoder` is a plain C pointer and is not
/// `Send`, which would make [`NativeScrcpyDecoder`] non-`Send` and break the
/// `VideoDecoder: Send` bound the pipeline relies on (the decoder is moved onto
/// the single decode worker thread; see [`crate::pipeline`]). This newtype makes
/// the owning decoder `Send` with the SAME discipline as [`SendScaler`].
///
/// # Safety
///
/// The `unsafe impl Send` below is sound HERE because of how the handle is
/// used, NOT because an `fb_decoder` is generally thread-safe (it is explicitly
/// NOT: see the threading note in `native/scrcpy-decode/fb_decoder.h`):
///
/// * [`NativeScrcpyDecoder`] owns this `FbDecoderHandle` exclusively (a private
///   field); it is never shared, cloned, aliased, or wrapped in an `Arc` handed
///   to another thread.
/// * The handle is only ever touched from the single decode worker thread
///   inside the [`VideoDecoder`] methods; there is no concurrent access.
/// * The only cross-thread action is MOVING the owning decoder (and with it the
///   pointer) onto the worker thread once, before any use. An exclusive move
///   followed by single-threaded use is fine: the raw pointer is just an owned
///   heap handle with no thread affinity.
#[cfg(feature = "native-decode")]
struct FbDecoderHandle(*mut scrcpy_decode_sys::fb_decoder);

// SAFETY: see the `FbDecoderHandle` doc comment above. The handle is owned
// exclusively by one `NativeScrcpyDecoder`, used only from the single decode
// worker thread, and merely moved (never shared) across the thread boundary.
#[cfg(feature = "native-decode")]
unsafe impl Send for FbDecoderHandle {}

#[cfg(feature = "native-decode")]
impl Drop for FbDecoderHandle {
    fn drop(&mut self) {
        // SAFETY: the pointer is either NULL (fb_decoder_free tolerates NULL) or
        // a live handle from fb_decoder_new that has not been freed yet; this
        // Drop is the single owner's sole release path.
        unsafe { scrcpy_decode_sys::fb_decoder_free(self.0) }
    }
}

/// The native scrcpy-based decoder: scrcpy's proven FFmpeg send/receive +
/// swscale-to-RGBA sequence with its `packet_merger` config handling, bound via
/// [`scrcpy_decode_sys`].
///
/// Compiled only under the `native-decode` feature because that feature links
/// FFmpeg and compiles the vendored scrcpy C (Windows/CI only; the headless
/// Linux host has no FFmpeg). It is the option-2b replacement for the brittle
/// pure-Rust path: the SPS/PPS config unit is PREPENDED to the next media
/// packet inside the engine (see
/// [`VideoDecoder::decode_unit`](fluxbridge_core::VideoDecoder::decode_unit)),
/// which is the fix for the `send_packet: invalid data found when processing
/// input` error.
///
/// It produces `DecodedFrame{ pixel_format: "rgba", data.len() == w*h*4 }`,
/// which the Slint UI renders directly.
#[cfg(feature = "native-decode")]
pub struct NativeScrcpyDecoder {
    handle: Option<FbDecoderHandle>,
    codec_fourcc: u32,
    width: u32,
    height: u32,
}

#[cfg(feature = "native-decode")]
impl NativeScrcpyDecoder {
    /// Create an unconfigured native decoder. The `fb_decoder` handle is built
    /// lazily on the first [`VideoDecoder::configure`] call, once the codec and
    /// dimensions are known.
    pub fn new() -> Self {
        NativeScrcpyDecoder {
            handle: None,
            codec_fourcc: u32::from_be_bytes(*b"h264"),
            width: 0,
            height: 0,
        }
    }

    /// Drain every currently available RGBA frame from the engine, returning the
    /// FIRST drawable one (if any) as a [`DecodedFrame`]. Any additional frames
    /// the decoder has buffered are consumed so they are not lost; the stream
    /// paces on arrival, so returning the first and draining the rest keeps the
    /// UI responsive without unbounded buffering.
    fn take_frame(&mut self) -> CoreResult<DecodedFrame> {
        use scrcpy_decode_sys::status;

        let handle = match &self.handle {
            Some(h) => h.0,
            None => {
                return Err(fluxbridge_core::CoreError::Other(
                    "native decoder not configured".into(),
                ))
            }
        };

        let mut first: Option<DecodedFrame> = None;
        loop {
            let mut out: *const u8 = std::ptr::null();
            let mut w: std::os::raw::c_int = 0;
            let mut h: std::os::raw::c_int = 0;
            let mut stride: std::os::raw::c_int = 0;
            // SAFETY: `handle` is a live fb_decoder owned exclusively by self and
            // only used from this worker thread. out/w/h/stride are valid local
            // pointers. On OK, `out` borrows a decoder-owned buffer valid until
            // the next fb_decoder_* call, so we copy it immediately below before
            // looping (which may invalidate it).
            let rc = unsafe {
                scrcpy_decode_sys::fb_decoder_take_rgba(
                    handle,
                    &mut out,
                    &mut w,
                    &mut h,
                    &mut stride,
                )
            };
            if rc == status::FB_DECODER_AGAIN {
                break;
            }
            if rc < 0 {
                return Err(fluxbridge_core::CoreError::Other(format!(
                    "fb_decoder_take_rgba failed: {rc}"
                )));
            }
            // rc == FB_DECODER_OK: a frame is available. Copy it out NOW.
            let w = w.max(0) as usize;
            let h = h.max(0) as usize;
            let stride = stride.max(0) as usize;
            if out.is_null() || w == 0 || h == 0 {
                continue;
            }
            // SAFETY: fb_decoder guarantees `out` points to at least
            // `stride * h` readable bytes (a tightly packed buffer with
            // stride == w*4) valid until the next fb_decoder_* call.
            let src = unsafe { std::slice::from_raw_parts(out, stride * h) };
            let data = pack_rgba(src, stride, w, h);
            if first.is_none() {
                first = Some(DecodedFrame {
                    width: w as u32,
                    height: h as u32,
                    pixel_format: "rgba".to_string(),
                    data,
                });
            }
            // Keep draining so buffered frames are not stranded.
        }

        Ok(first.unwrap_or_else(|| DecodedFrame {
            width: self.width,
            height: self.height,
            pixel_format: "rgba".to_string(),
            data: Vec::new(),
        }))
    }
}

#[cfg(feature = "native-decode")]
impl Default for NativeScrcpyDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "native-decode")]
impl VideoDecoder for NativeScrcpyDecoder {
    fn configure(&mut self, config: &VideoConfig) -> CoreResult<()> {
        // Remember the codec/dimensions. Only non-zero dimensions update the
        // stored geometry, so a later zero-dim config (a stray call) never
        // clobbers the known-good header geometry, mirroring FfmpegDecoder.
        let fourcc = codec_fourcc(&config.codec);
        if fourcc != 0 {
            self.codec_fourcc = fourcc;
        }
        if config.width != 0 && config.height != 0 {
            self.width = config.width;
            self.height = config.height;
        }
        // Build the handle lazily the first time (idempotent on re-configure:
        // scrcpy sends SPS/PPS inline with keyframes, so the engine learns real
        // geometry from the bitstream and need not be rebuilt mid-stream).
        if self.handle.is_none() {
            // SAFETY: fb_decoder_new allocates and opens the codec; it returns
            // NULL on failure, which we map to an error. The returned pointer is
            // owned exclusively by this decoder and freed in FbDecoderHandle's
            // Drop.
            let raw = unsafe {
                scrcpy_decode_sys::fb_decoder_new(self.codec_fourcc, self.width, self.height)
            };
            if raw.is_null() {
                return Err(fluxbridge_core::CoreError::Other(
                    "fb_decoder_new returned NULL (codec open failed)".into(),
                ));
            }
            self.handle = Some(FbDecoderHandle(raw));
        }
        Ok(())
    }

    fn decode(&mut self, encoded: &[u8]) -> CoreResult<DecodedFrame> {
        // A bare decode() is a media (non-config) unit with unknown keyframe
        // status; route it through the flag-carrying seam.
        self.decode_unit(encoded, false, false)
    }

    fn decode_unit(
        &mut self,
        encoded: &[u8],
        is_config: bool,
        is_keyframe: bool,
    ) -> CoreResult<DecodedFrame> {
        use scrcpy_decode_sys::status;

        let handle = match &self.handle {
            Some(h) => h.0,
            None => {
                return Err(fluxbridge_core::CoreError::Other(
                    "native decoder not configured before decode".into(),
                ))
            }
        };

        let (ptr, len) = (encoded.as_ptr(), encoded.len());
        // SAFETY: `handle` is a live fb_decoder owned exclusively by self and
        // only used from this worker thread. `ptr`/`len` describe a valid
        // readable slice for the duration of the call. The engine runs the
        // packet merger so a config unit is held and prepended to the next
        // media packet.
        let rc = unsafe {
            scrcpy_decode_sys::fb_decoder_feed(
                handle,
                ptr,
                len,
                is_config as std::os::raw::c_int,
                is_keyframe as std::os::raw::c_int,
            )
        };
        if rc < 0 {
            return Err(fluxbridge_core::CoreError::Other(format!(
                "fb_decoder_feed failed: {rc}"
            )));
        }
        // FB_DECODER_AGAIN (config held, or no frame yet) yields an empty frame
        // the stream driver skips; FB_DECODER_OK means frames may be ready.
        if rc == status::FB_DECODER_AGAIN {
            return Ok(DecodedFrame {
                width: self.width,
                height: self.height,
                pixel_format: "rgba".to_string(),
                data: Vec::new(),
            });
        }
        self.take_frame()
    }

    fn flush(&mut self) -> CoreResult<()> {
        // The engine drains on take; there is no separate libav flush exposed
        // over the C API, so drop any pending frames best-effort. Recreating the
        // handle on the next configure is the reset path.
        if self.handle.is_some() {
            let _ = self.take_frame();
        }
        Ok(())
    }
}

/// Map a codec name (for example `"h264"`, `"h265"`, `"hevc"`) to the scrcpy
/// fourcc `u32` the native engine expects. Unknown names map to `0` so the
/// caller keeps the previously stored codec rather than overwriting it.
#[cfg(feature = "native-decode")]
fn codec_fourcc(codec: &str) -> u32 {
    match codec {
        "h264" => u32::from_be_bytes(*b"h264"),
        "h265" | "hevc" => u32::from_be_bytes(*b"h265"),
        "av1" => u32::from_be_bytes(*b"av01"),
        _ => 0,
    }
}

/// Run the FluxBridge-native (`0xFB`) decode loop until the packet channel
/// closes.
///
/// This consumes [`PacketType::VideoFrame`] / [`PacketType::VideoConfig`]
/// packets off a FluxBridge [`crate::connection::Connection`]. It is NOT used on
/// the shipped v1 scrcpy mirroring path (which uses
/// [`crate::scrcpy::stream_av_packets`]); it is kept for a future
/// FluxBridge-native capture server that speaks the `0xFB` protocol.
///
/// For every packet received:
/// * [`PacketType::VideoConfig`] payloads (re)configure the decoder. The config
///   bytes are not parsed yet; a fixed [`VideoConfig`] is used so the stub has
///   sane dimensions. A native server will carry the config payload defined in
///   `docs/PROTOCOL.md`.
/// * [`PacketType::VideoFrame`] payloads are decoded and the resulting
///   [`DecodedFrame`] is pushed to `frame_sink`.
/// * All other packet types are ignored here (handled elsewhere).
///
/// Returns when the `packets` sender is dropped (connection closed) or the
/// `frame_sink` receiver is gone (UI shut down).
pub fn run_decode_loop(
    packets: &Receiver<Packet>,
    decoder: &mut dyn VideoDecoder,
    frame_sink: &Sender<DecodedFrame>,
) -> CoreResult<()> {
    // Ensure the decoder has a configuration even if the server never sends an
    // explicit VideoConfig in this scaffold.
    let default_config = VideoConfig {
        codec: "h264".to_string(),
        width: 1080,
        height: 1920,
        extra_data: Vec::new(),
    };
    decoder.configure(&default_config)?;

    while let Ok(packet) = packets.recv() {
        match packet.packet_type {
            PacketType::VideoConfig => {
                // Real impl: parse the codec/resolution from packet.payload.
                decoder.configure(&default_config)?;
            }
            PacketType::VideoFrame => {
                let frame = decoder.decode(&packet.payload)?;
                if frame_sink.send(frame).is_err() {
                    break; // UI gone
                }
            }
            _ => {
                // Non-video packets are routed by other consumers.
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A fake decoder that records how many frames it decoded and tags output.
    #[derive(Default)]
    struct FakeDecoder {
        decoded: usize,
        configured: bool,
    }

    impl VideoDecoder for FakeDecoder {
        fn configure(&mut self, _config: &VideoConfig) -> CoreResult<()> {
            self.configured = true;
            Ok(())
        }

        fn decode(&mut self, encoded: &[u8]) -> CoreResult<DecodedFrame> {
            self.decoded += 1;
            Ok(DecodedFrame {
                width: 4,
                height: 4,
                pixel_format: "fake".to_string(),
                data: encoded.to_vec(),
            })
        }

        fn flush(&mut self) -> CoreResult<()> {
            Ok(())
        }
    }

    #[test]
    fn drives_fake_frames_through_fake_decoder() {
        let (pkt_tx, pkt_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::channel();

        // Queue a config packet and three video frames, then close the sender.
        pkt_tx
            .send(Packet::new(PacketType::VideoConfig, vec![]))
            .unwrap();
        for i in 0..3u8 {
            pkt_tx
                .send(Packet::new(PacketType::VideoFrame, vec![i, i + 1, i + 2]))
                .unwrap();
        }
        // A non-video packet that must be ignored by the loop.
        pkt_tx
            .send(Packet::new(PacketType::Heartbeat, vec![]))
            .unwrap();
        drop(pkt_tx);

        let mut decoder = FakeDecoder::default();
        run_decode_loop(&pkt_rx, &mut decoder, &frame_tx).unwrap();

        assert!(decoder.configured, "decoder should be configured");
        assert_eq!(decoder.decoded, 3, "three video frames should be decoded");

        let frames: Vec<DecodedFrame> = frame_rx.try_iter().collect();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].data, vec![0, 1, 2]);
        assert_eq!(frames[2].data, vec![2, 3, 4]);
        assert_eq!(frames[0].pixel_format, "fake");
    }

    #[test]
    fn pack_rgba_no_padding_is_identity() {
        // src_stride == width*4: output should equal the input, byte for byte.
        let width = 3;
        let height = 2;
        let stride = width * 4;
        let src: Vec<u8> = (0..(stride * height) as u8).collect();
        let out = pack_rgba(&src, stride, width, height);
        assert_eq!(out.len(), width * height * 4);
        assert_eq!(out, src, "no-padding pack must be the identity");
    }

    #[test]
    fn pack_rgba_strips_row_padding() {
        // src_stride > width*4: trailing padding bytes on each row are dropped.
        let width = 2; // 2 px => 8 visible bytes per row
        let height = 3;
        let row_bytes = width * 4; // 8
        let stride = row_bytes + 4; // 4 bytes of padding per row
        let mut src = Vec::new();
        for row in 0..height {
            // Visible pixels: a distinct value per row so we can assert ordering.
            for b in 0..row_bytes {
                src.push((row * 10 + b) as u8);
            }
            // Padding bytes we must NOT see in the output.
            src.extend_from_slice(&[0xFF, 0xFE, 0xFD, 0xFC]);
        }
        let out = pack_rgba(&src, stride, width, height);
        assert_eq!(out.len(), width * height * 4);
        // Expected is the concatenation of just the visible bytes, in row order.
        let mut expected = Vec::new();
        for row in 0..height {
            for b in 0..row_bytes {
                expected.push((row * 10 + b) as u8);
            }
        }
        assert_eq!(
            out, expected,
            "padding must be stripped, rows kept in order"
        );
        assert!(
            !out.contains(&0xFF) && !out.contains(&0xFE),
            "padding bytes must not leak into the packed buffer"
        );
    }

    #[test]
    fn pack_rgba_one_by_one() {
        // A single pixel with padding past it: exactly 4 bytes come out.
        let src = vec![1, 2, 3, 4, 99, 99, 99, 99];
        let out = pack_rgba(&src, 8, 1, 1);
        assert_eq!(out, vec![1, 2, 3, 4]);
    }

    #[test]
    fn pack_rgba_multi_row_ordering_unpadded() {
        // 1px wide, 3 rows, no padding: verify top-to-bottom row ordering.
        let src = vec![
            10, 11, 12, 13, // row 0
            20, 21, 22, 23, // row 1
            30, 31, 32, 33, // row 2
        ];
        let out = pack_rgba(&src, 4, 1, 3);
        assert_eq!(out, src);
    }

    #[test]
    fn pack_rgba_output_satisfies_ui_length_invariant() {
        // Document the decoder -> UI contract: a DecodedFrame carrying a
        // pack_rgba buffer has data.len() == width*height*4, which is exactly
        // the length ui::decoded_frame_to_image requires to draw a frame.
        let width = 5;
        let height = 4;
        let stride = width * 4 + 12; // some arbitrary padding
        let src = vec![7u8; stride * height];
        let frame = DecodedFrame {
            width: width as u32,
            height: height as u32,
            pixel_format: "rgba".to_string(),
            data: pack_rgba(&src, stride, width, height),
        };
        assert_eq!(frame.pixel_format, "rgba");
        assert_eq!(
            frame.data.len(),
            (frame.width as usize) * (frame.height as usize) * 4
        );
    }

    /// A fake that EMULATES the native feed/take sequencing without any FFI, so
    /// the rgba `DecodedFrame` construction and its `data.len() == w*h*4`
    /// invariant (the exact contract ui::decoded_frame_to_image requires to
    /// draw) can be exercised headlessly with `native-decode` OFF.
    ///
    /// It mirrors `NativeScrcpyDecoder`'s real control flow: a config unit is
    /// held (empty frame, no pixels); a media unit is "fed", and the next
    /// `take` produces one tightly packed RGBA frame sized to the configured
    /// geometry, stripped of a simulated row stride via the same `pack_rgba`
    /// used by the real decoder. This would fail if the length math or the
    /// config/media sequencing regressed.
    #[derive(Default)]
    struct FakeFeedTakeDecoder {
        width: u32,
        height: u32,
        /// A frame is pending take after a media feed (config feeds hold).
        pending: bool,
    }

    impl VideoDecoder for FakeFeedTakeDecoder {
        fn configure(&mut self, config: &VideoConfig) -> CoreResult<()> {
            if config.width != 0 && config.height != 0 {
                self.width = config.width;
                self.height = config.height;
            }
            Ok(())
        }

        fn decode(&mut self, encoded: &[u8]) -> CoreResult<DecodedFrame> {
            self.decode_unit(encoded, false, false)
        }

        fn decode_unit(
            &mut self,
            _encoded: &[u8],
            is_config: bool,
            _is_keyframe: bool,
        ) -> CoreResult<DecodedFrame> {
            if is_config {
                // Config held for merge: no drawable frame yet.
                return Ok(DecodedFrame {
                    width: self.width,
                    height: self.height,
                    pixel_format: "rgba".to_string(),
                    data: Vec::new(),
                });
            }
            // "Feed" then "take": produce one RGBA frame. Simulate a padded
            // source stride so pack_rgba's padding-strip path is exercised,
            // exactly as the real engine's take_rgba does.
            self.pending = true;
            let w = self.width as usize;
            let h = self.height as usize;
            let stride = w * 4 + 8; // 8 bytes of row padding
            let src = vec![0x5Au8; stride * h];
            let data = pack_rgba(&src, stride, w, h);
            self.pending = false;
            Ok(DecodedFrame {
                width: self.width,
                height: self.height,
                pixel_format: "rgba".to_string(),
                data,
            })
        }

        fn flush(&mut self) -> CoreResult<()> {
            Ok(())
        }
    }

    #[test]
    fn native_feed_take_rgba_frame_satisfies_length_invariant() {
        let mut dec = FakeFeedTakeDecoder::default();
        dec.configure(&VideoConfig {
            codec: "h264".into(),
            width: 64,
            height: 48,
            extra_data: vec![],
        })
        .unwrap();

        // A config unit yields no drawable frame (held for merge).
        let held = dec.decode_unit(&[0x67, 0x42], true, false).unwrap();
        assert!(
            held.data.is_empty(),
            "a config unit must not produce drawable pixels"
        );

        // The first media unit produces a frame whose byte length is exactly
        // width*height*4 (the UI render gate), with the stride padding stripped.
        let frame = dec.decode_unit(&[0x65, 0x88], false, true).unwrap();
        assert_eq!(frame.pixel_format, "rgba");
        assert_eq!(frame.width, 64);
        assert_eq!(frame.height, 48);
        assert_eq!(
            frame.data.len(),
            (frame.width as usize) * (frame.height as usize) * 4,
            "rgba DecodedFrame must satisfy data.len() == w*h*4"
        );
    }

    #[test]
    fn stub_decoder_echoes_payload() {
        let mut decoder = StubDecoder::new();
        decoder
            .configure(&VideoConfig {
                codec: "h264".into(),
                width: 100,
                height: 200,
                extra_data: vec![],
            })
            .unwrap();
        let frame = decoder.decode(&[9, 8, 7]).unwrap();
        assert_eq!(frame.width, 100);
        assert_eq!(frame.height, 200);
        assert_eq!(frame.data, vec![9, 8, 7]);
    }
}
