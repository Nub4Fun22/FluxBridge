/*
 * fb_decoder.h
 *
 * FluxBridge native decode engine: a thin, self-contained C API that exposes
 * scrcpy's PROVEN H.264/H.265 decode sequence for use over FFI from Rust.
 *
 * This header is FluxBridge-original. It is NOT vendored from scrcpy. The
 * implementation (fb_decoder.c, landed in FEAT-003) REIMPLEMENTS the avcodec
 * send/receive + swscale-to-RGBA sequence from scrcpy v5.0.1
 * app/src/decoder.c::sc_decoder_process_packet (avcodec_send_packet followed by
 * a loop of avcodec_receive_frame, with AV_CODEC_FLAG_LOW_DELAY) and the config
 * handling from app/src/packet_merger.c (vendored verbatim under
 * native/scrcpy-decode/vendor/, see ../NOTICE). The 12-byte scrcpy frame header
 * framing from app/src/demuxer.c is parsed on the Rust side before calling
 * fb_decoder_feed; only the per-access-unit payload and its config/keyframe
 * flags are passed across this API.
 *
 * Design goals:
 *   - C ABI only. No scrcpy, SDL, or FFmpeg types appear in this interface;
 *     the handle is opaque so Rust bindgen sees a stable surface.
 *   - Matches the fluxbridge_core::VideoDecoder trait seam
 *     (configure / decode / flush) and the Slint render contract: the Rust
 *     wrapper produces DecodedFrame{pixel_format:"rgba", data.len()==w*h*4}.
 *
 * Threading: a fb_decoder_t handle is NOT thread-safe. It is created, fed,
 * drained, and freed from a single thread (FluxBridge's stream worker thread),
 * mirroring the SendScaler single-owner note in
 * crates/desktop/src/decode_loop.rs.
 */

#ifndef FB_DECODER_H
#define FB_DECODER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Return codes shared by the feed/take entry points.
 *
 * Values are stable and may be matched by the Rust FFI wrapper. Negative values
 * are fatal; non-negative values are normal, recoverable flow.
 */
enum fb_decoder_status {
    /* A unit was accepted / a frame was produced. */
    FB_DECODER_OK = 0,
    /* The call succeeded but no output is available yet (decoder needs more
     * input, e.g. a config-only unit was merged and is awaiting its media
     * packet, or avcodec returned AVERROR(EAGAIN)). Not an error. */
    FB_DECODER_AGAIN = 1,
    /* Invalid argument (NULL handle, NULL data with non-zero len, etc.). */
    FB_DECODER_ERR_INVALID = -1,
    /* Out-of-memory while buffering the config packet or growing a packet. */
    FB_DECODER_ERR_OOM = -2,
    /* A libav call failed in a way the stream cannot recover from. */
    FB_DECODER_ERR_FATAL = -3,
};

/**
 * Opaque decoder handle. Allocated by fb_decoder_new, released by
 * fb_decoder_free. Never dereferenced by the caller.
 */
typedef struct fb_decoder fb_decoder_t;

/**
 * Create a decoder for the given stream.
 *
 * @param codec_fourcc Little-meaning packed FourCC identifying the codec, as
 *                     delivered in the scrcpy codec id, e.g. 'h264' or 'h265'
 *                     (built as ('h'<<24)|('2'<<16)|('6'<<8)|'4'). The
 *                     implementation maps this to an AVCodecID.
 * @param width        Frame width from the scrcpy session header. May be 0 if
 *                     unknown; the decoder learns true dimensions from the
 *                     bitstream and reports them via fb_decoder_take_rgba.
 * @param height       Frame height from the scrcpy session header. May be 0.
 * @return a new handle, or NULL on allocation/codec-open failure.
 *
 * Ownership: caller owns the returned handle and must release it with
 * fb_decoder_free.
 */
fb_decoder_t *fb_decoder_new(uint32_t codec_fourcc, uint32_t width, uint32_t height);

/**
 * Feed exactly ONE scrcpy access unit (one demuxed payload).
 *
 * Internally this runs the vendored packet_merger: when is_config is non-zero
 * the unit's bytes (SPS/PPS) are REMEMBERED, not decoded, and the call returns
 * FB_DECODER_AGAIN. When the next media unit arrives, the remembered config
 * bytes are PREPENDED to it so avcodec_send_packet receives a single
 * [SPS/PPS][keyframe] Annex-B AVPacket. This is the exact correctness fix for
 * the `send_packet: invalid data found when processing input` error that the
 * old pure-Rust path produced by calling configure() with empty dimensions and
 * a separate extradata buffer.
 *
 * @param d           decoder handle.
 * @param data        pointer to the access-unit payload (Annex-B). Must be
 *                    non-NULL when len > 0.
 * @param len         payload length in bytes.
 * @param is_config   non-zero if this is a config (SPS/PPS) unit
 *                    (scrcpy frame-header config bit; pts == AV_NOPTS_VALUE).
 * @param is_keyframe non-zero if this is a keyframe/IDR unit
 *                    (scrcpy frame-header key bit). Informational; the merge
 *                    does not depend on it.
 * @return FB_DECODER_OK when the unit was sent to the decoder,
 *         FB_DECODER_AGAIN when the unit was a config unit held for merging,
 *         or a negative fb_decoder_status on error.
 *
 * After FB_DECODER_OK, the caller should drain frames with
 * fb_decoder_take_rgba until it returns FB_DECODER_AGAIN.
 */
int fb_decoder_feed(fb_decoder_t *d, const uint8_t *data, size_t len,
                    int is_config, int is_keyframe);

/**
 * Pull the next decoded frame, converted to tightly packed RGBA via swscale.
 *
 * @param d      decoder handle.
 * @param out    receives a pointer to the decoder-owned RGBA buffer. The buffer
 *               is valid ONLY until the next call to fb_decoder_feed,
 *               fb_decoder_take_rgba, or fb_decoder_free on the same handle. The
 *               Rust wrapper MUST copy it into an owned DecodedFrame before the
 *               next call. The caller must NOT free it.
 * @param w      receives the frame width in pixels.
 * @param h      receives the frame height in pixels.
 * @param stride receives the row stride in bytes of the RGBA buffer. The
 *               implementation targets a tightly packed buffer (stride == w*4);
 *               the stride is reported so the Rust side can strip any swscale
 *               row padding exactly as crates/desktop/src/decode_loop.rs
 *               ::pack_rgba already does.
 * @return FB_DECODER_OK when a frame was produced (out/w/h/stride are set),
 *         FB_DECODER_AGAIN when no frame is currently available (out is left
 *         untouched), or a negative fb_decoder_status on error.
 */
int fb_decoder_take_rgba(fb_decoder_t *d, const uint8_t **out, int *w, int *h,
                         int *stride);

/**
 * Release a decoder handle and all associated libav/swscale resources,
 * including any buffered config packet. Safe to call with NULL.
 */
void fb_decoder_free(fb_decoder_t *d);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* FB_DECODER_H */
