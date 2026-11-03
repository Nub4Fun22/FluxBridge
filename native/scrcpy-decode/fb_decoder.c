/*
 * fb_decoder.c
 *
 * FluxBridge native decode engine implementation.
 *
 * This file is FluxBridge-original. It REIMPLEMENTS the avcodec send/receive +
 * swscale-to-RGBA sequence from scrcpy v5.0.1 app/src/decoder.c
 * (sc_decoder_process_packet: avcodec_send_packet followed by a loop of
 * avcodec_receive_frame, with AV_CODEC_FLAG_LOW_DELAY set on the context) and
 * reuses scrcpy's config (SPS/PPS) handling verbatim via the vendored
 * packet_merger (native/scrcpy-decode/vendor/packet_merger.c, see ../NOTICE).
 * It is self-contained against libavcodec / libavutil / libswscale only: no
 * scrcpy util/trait/thread deps, no SDL, no hwdec selection.
 *
 * It is COMPILED ONLY on Windows CI (which provisions LLVM + BtbN FFmpeg 7.1.3
 * shared) and finally exercised on the user's device. It is NOT compiled on the
 * headless Linux host, which has no FFmpeg; there it is reviewed by inspection.
 *
 * Correctness note (the whole point of option 2b): a config unit is NOT decoded
 * on its own and NOT delivered via a separate "configure with empty dims" call.
 * sc_packet_merger_merge remembers the config packet (pts == AV_NOPTS_VALUE)
 * and PREPENDS it to the next media packet, so avcodec_send_packet receives a
 * single [SPS/PPS][keyframe] Annex-B AVPacket. This is the exact fix for the
 * `send_packet: invalid data found when processing input` error the old
 * pure-Rust path produced.
 */

#include "fb_decoder.h"

#include <stdlib.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavcodec/packet.h>
#include <libavutil/avutil.h>
#include <libavutil/frame.h>
#include <libavutil/imgutils.h>
#include <libavutil/pixfmt.h>
#include <libswscale/swscale.h>

#include "vendor/packet_merger.h"

/*
 * Opaque decoder state. Owned exclusively by the Rust NativeScrcpyDecoder on a
 * single thread (see the threading note in fb_decoder.h); no locking is needed.
 */
struct fb_decoder {
    const AVCodec *codec;
    AVCodecContext *ctx;
    struct sc_packet_merger merger;
    AVPacket *packet;       /* reused input packet */
    AVFrame *frame;         /* reused decoded frame (YUV) */
    struct SwsContext *sws; /* lazily (re)built on geometry/pixfmt change */
    /* The (w, h, src_pix_fmt) the current sws context was built for, so it is
     * rebuilt when the decoded geometry or pixel format changes mid-stream
     * (device rotation / fold), mirroring FfmpegDecoder's scaler_src. */
    int sws_w;
    int sws_h;
    enum AVPixelFormat sws_src_fmt;
    /* Tightly packed RGBA output buffer (stride == w*4). Grown as needed and
     * handed out (borrowed) by fb_decoder_take_rgba until the next call. */
    uint8_t *rgba;
    int rgba_w;
    int rgba_h;
    size_t rgba_cap;
    /* Whether a frame is currently sitting in `frame` waiting to be converted
     * and taken. Set by a successful avcodec_receive_frame in feed-drain. */
    int have_pending_frame;
};

/* Map the scrcpy codec fourcc (e.g. ('h'<<24)|('2'<<16)|('6'<<8)|'4') to an
 * AVCodecID. scrcpy requests H.264 by default and can also stream H.265. */
static enum AVCodecID
fb_codec_id_from_fourcc(uint32_t fourcc) {
    uint8_t c0 = (uint8_t) (fourcc >> 24);
    uint8_t c1 = (uint8_t) (fourcc >> 16);
    uint8_t c2 = (uint8_t) (fourcc >> 8);
    uint8_t c3 = (uint8_t) fourcc;

    if (c0 == 'h' && c1 == '2' && c2 == '6' && c3 == '4') {
        return AV_CODEC_ID_H264;
    }
    if ((c0 == 'h' && c1 == '2' && c2 == '6' && c3 == '5') ||
        (c0 == 'h' && c1 == 'e' && c2 == 'v' && c3 == 'c')) {
        return AV_CODEC_ID_HEVC;
    }
    if (c0 == 'a' && c1 == 'v' && c2 == '0' && c3 == '1') {
        return AV_CODEC_ID_AV1;
    }
    /* Default to H.264: it is the only codec FluxBridge requests today. */
    return AV_CODEC_ID_H264;
}

fb_decoder_t *
fb_decoder_new(uint32_t codec_fourcc, uint32_t width, uint32_t height) {
    fb_decoder_t *d = calloc(1, sizeof(*d));
    if (!d) {
        return NULL;
    }

    enum AVCodecID codec_id = fb_codec_id_from_fourcc(codec_fourcc);
    d->codec = avcodec_find_decoder(codec_id);
    if (!d->codec) {
        free(d);
        return NULL;
    }

    d->ctx = avcodec_alloc_context3(d->codec);
    if (!d->ctx) {
        free(d);
        return NULL;
    }

    /* scrcpy decodes with low delay so the client shows frames as soon as they
     * are produced instead of buffering reorder frames; see decoder.c. */
    d->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;

    /* Seed the context geometry from the scrcpy session header when known. The
     * decoder learns the true dimensions from the bitstream regardless; these
     * are only a hint, so a 0 (unknown) is fine. */
    if (width > 0 && height > 0) {
        d->ctx->width = (int) width;
        d->ctx->height = (int) height;
    }

    if (avcodec_open2(d->ctx, d->codec, NULL) < 0) {
        avcodec_free_context(&d->ctx);
        free(d);
        return NULL;
    }

    d->packet = av_packet_alloc();
    d->frame = av_frame_alloc();
    if (!d->packet || !d->frame) {
        av_packet_free(&d->packet);
        av_frame_free(&d->frame);
        avcodec_free_context(&d->ctx);
        free(d);
        return NULL;
    }

    sc_packet_merger_init(&d->merger);
    d->sws = NULL;
    d->sws_w = 0;
    d->sws_h = 0;
    d->sws_src_fmt = AV_PIX_FMT_NONE;
    d->rgba = NULL;
    d->rgba_cap = 0;
    d->rgba_w = 0;
    d->rgba_h = 0;
    d->have_pending_frame = 0;
    return d;
}

int
fb_decoder_feed(fb_decoder_t *d, const uint8_t *data, size_t len,
                int is_config, int is_keyframe) {
    (void) is_keyframe; /* informational; the merge does not depend on it */

    if (!d) {
        return FB_DECODER_ERR_INVALID;
    }
    if (len > 0 && !data) {
        return FB_DECODER_ERR_INVALID;
    }

    /* Build an AVPacket for this access unit. av_new_packet allocates a
     * ref-counted, padded buffer that av_grow_packet (used by the merger) can
     * grow in place, which is exactly what scrcpy's demuxer relies on. */
    if (av_new_packet(d->packet, (int) len) < 0) {
        return FB_DECODER_ERR_OOM;
    }
    if (len > 0) {
        memcpy(d->packet->data, data, len);
    }

    /* scrcpy marks a config packet with pts == AV_NOPTS_VALUE; the merger keys
     * off exactly that. A media packet gets a monotonically increasing dummy
     * pts (0 is fine for a low-delay decode; real pts is not needed to render
     * since FluxBridge paces on arrival). */
    d->packet->pts = is_config ? AV_NOPTS_VALUE : 0;
    d->packet->dts = d->packet->pts;

    /* Run scrcpy's proven merge: a config packet is remembered and a following
     * media packet has the config bytes prepended, in place. */
    if (!sc_packet_merger_merge(&d->merger, d->packet)) {
        av_packet_unref(d->packet);
        return FB_DECODER_ERR_OOM;
    }

    if (is_config) {
        /* The config bytes were buffered by the merger for the next media
         * packet; nothing to send yet. */
        av_packet_unref(d->packet);
        return FB_DECODER_AGAIN;
    }

    int ret = avcodec_send_packet(d->ctx, d->packet);
    av_packet_unref(d->packet);
    if (ret < 0 && ret != AVERROR(EAGAIN)) {
        return FB_DECODER_ERR_FATAL;
    }

    /* Try to pull one frame now so take_rgba has something to convert. More
     * frames (if the decoder buffered several) are drained by repeated
     * take_rgba calls. */
    ret = avcodec_receive_frame(d->ctx, d->frame);
    if (ret == 0) {
        d->have_pending_frame = 1;
        return FB_DECODER_OK;
    }
    if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) {
        return FB_DECODER_AGAIN;
    }
    return FB_DECODER_ERR_FATAL;
}

/* Ensure the RGBA output buffer can hold a tightly packed w*h*4 image. */
static int
fb_ensure_rgba(fb_decoder_t *d, int w, int h) {
    size_t need = (size_t) w * (size_t) h * 4u;
    if (need > d->rgba_cap) {
        uint8_t *grown = realloc(d->rgba, need);
        if (!grown) {
            return FB_DECODER_ERR_OOM;
        }
        d->rgba = grown;
        d->rgba_cap = need;
    }
    d->rgba_w = w;
    d->rgba_h = h;
    return FB_DECODER_OK;
}

/* Convert the frame currently in d->frame to tightly packed RGBA. */
static int
fb_convert_pending(fb_decoder_t *d, const uint8_t **out, int *w, int *h,
                   int *stride) {
    int fw = d->frame->width;
    int fh = d->frame->height;
    enum AVPixelFormat src_fmt = (enum AVPixelFormat) d->frame->format;

    /* (Re)build the sws context whenever the decoded geometry or pixel format
     * differs from what the cached context was built for (mid-stream rotation
     * / fold), mirroring FfmpegDecoder's scaler_src handling. */
    if (!d->sws || d->sws_w != fw || d->sws_h != fh ||
        d->sws_src_fmt != src_fmt) {
        if (d->sws) {
            sws_freeContext(d->sws);
            d->sws = NULL;
        }
        d->sws = sws_getContext(fw, fh, src_fmt, fw, fh, AV_PIX_FMT_RGBA,
                                SWS_BILINEAR, NULL, NULL, NULL);
        if (!d->sws) {
            return FB_DECODER_ERR_FATAL;
        }
        d->sws_w = fw;
        d->sws_h = fh;
        d->sws_src_fmt = src_fmt;
    }

    int rc = fb_ensure_rgba(d, fw, fh);
    if (rc != FB_DECODER_OK) {
        return rc;
    }

    /* Tightly packed destination: one plane, stride == w*4. */
    uint8_t *dst_data[4] = {d->rgba, NULL, NULL, NULL};
    int dst_stride[4] = {fw * 4, 0, 0, 0};

    int scaled = sws_scale(d->sws, (const uint8_t *const *) d->frame->data,
                           d->frame->linesize, 0, fh, dst_data, dst_stride);
    if (scaled < 0) {
        return FB_DECODER_ERR_FATAL;
    }

    *out = d->rgba;
    *w = fw;
    *h = fh;
    *stride = fw * 4;
    return FB_DECODER_OK;
}

int
fb_decoder_take_rgba(fb_decoder_t *d, const uint8_t **out, int *w, int *h,
                     int *stride) {
    if (!d || !out || !w || !h || !stride) {
        return FB_DECODER_ERR_INVALID;
    }

    /* If feed() already pulled a frame, convert it. Otherwise try to pull the
     * next buffered frame (the decoder may have produced more than one). */
    if (!d->have_pending_frame) {
        int ret = avcodec_receive_frame(d->ctx, d->frame);
        if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) {
            return FB_DECODER_AGAIN;
        }
        if (ret < 0) {
            return FB_DECODER_ERR_FATAL;
        }
    }
    d->have_pending_frame = 0;

    int rc = fb_convert_pending(d, out, w, h, stride);
    av_frame_unref(d->frame);
    return rc;
}

void
fb_decoder_free(fb_decoder_t *d) {
    if (!d) {
        return;
    }
    sc_packet_merger_destroy(&d->merger);
    if (d->sws) {
        sws_freeContext(d->sws);
    }
    av_frame_free(&d->frame);
    av_packet_free(&d->packet);
    avcodec_free_context(&d->ctx);
    free(d->rgba);
    free(d);
}
