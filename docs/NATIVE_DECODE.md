# Native decode engine (option 2b)

This document specifies FluxBridge's native video decode engine: a small amount
of vendored and reimplemented C that builds on scrcpy's proven decode algorithm,
exposed over a thin C ABI and driven from Rust behind an off-by-default cargo
feature. It is the design for the re-architecture the user chose as "option 2b".

## 1. Decision and rationale

FluxBridge previously reimplemented scrcpy's client-side H.264/H.265 demux and
decode in pure Rust. That path produced an unresolved
`send_packet: invalid data found when processing input` error because the
config (SPS/PPS) access unit was delivered through a separate
`configure()` call with empty dimensions and a standalone extradata buffer,
instead of being prepended to the next media packet the way scrcpy does.

The decision: stop reimplementing scrcpy's decode from scratch. Instead:

- Vendor the single small, proven piece FluxBridge got wrong (scrcpy's
  `packet_merger`) verbatim.
- Reimplement only scrcpy's FFmpeg send/receive + swscale-to-RGBA sequence in a
  small self-contained C wrapper (`fb_decoder`), citing scrcpy's `decoder.c`
  and `demuxer.c` as the reference rather than copying their util-entangled code.
- Render the decoded RGBA frames inside FluxBridge's existing Slint in-window
  surface, NOT in a separate SDL/scrcpy window.

Rationale: the merge logic is the proven fix for the exact bug, and keeping the
rest as a reimplementation avoids dragging in scrcpy's hardware-decode
selection, trait layer, and threading utilities that FluxBridge does not need.

## 2. What is vendored vs reimplemented

### Vendored verbatim (include-only trim)

- `native/scrcpy-decode/vendor/packet_merger.h`
  (from scrcpy v5.0.1 `app/src/packet_merger.h`)
- `native/scrcpy-decode/vendor/packet_merger.c`
  (from scrcpy v5.0.1 `app/src/packet_merger.c`)

Only the include lines were changed, to decouple the files from scrcpy's
internal headers so they compile standalone against libavcodec / libavutil. The
merge algorithm is unchanged. See `native/scrcpy-decode/VENDOR.md` and
`native/scrcpy-decode/NOTICE` for the exact modifications and the upstream
commit sha.

The algorithm: a config packet (`pts == AV_NOPTS_VALUE`) is remembered; the
next media packet is grown and the config bytes are memmoved/memcpy'd to the
front, so the decoder receives one `[SPS/PPS][keyframe]` Annex-B `AVPacket`.

### Reimplemented from reference (NOT vendored)

- The avcodec sequence, from scrcpy `app/src/decoder.c`
  (`sc_decoder_process_packet`): `avcodec_send_packet` followed by a loop of
  `avcodec_receive_frame`, with `AV_CODEC_FLAG_LOW_DELAY` set on the codec
  context.
- The 12-byte frame-header framing, from scrcpy `app/src/demuxer.c`:
  an 8-byte pts/flags field (bit 62 = config, bit 61 = keyframe,
  bit 63 / MSB = session header carrying width/height) plus a 4-byte payload
  length, and the `AVCodecParameters` setup (codec id, width/height,
  `AV_PIX_FMT_YUV420P`).

### Why the rest of scrcpy is not needed

scrcpy's `decoder.c` / `demuxer.c` are entangled with scrcpy's own util layer:
hardware-decode selection (`sc_hwdec`), the `frame_source` / `packet_sink`
traits, `util/vecdeque`, `util/thread` mutex/cond, the stream session,
`container_of`, and custom logging. FluxBridge already owns its stream worker
thread, status plumbing, and render surface, so none of that is required. Only
the decode core is reused.

## 3. The fb_decoder.h C API surface

Defined in `native/scrcpy-decode/fb_decoder.h`. C ABI only; no scrcpy, SDL, or
FFmpeg types leak through the interface; the handle is opaque.

```c
typedef struct fb_decoder fb_decoder_t;

fb_decoder_t *fb_decoder_new(uint32_t codec_fourcc, uint32_t width, uint32_t height);
int fb_decoder_feed(fb_decoder_t *d, const uint8_t *data, size_t len,
                    int is_config, int is_keyframe);
int fb_decoder_take_rgba(fb_decoder_t *d, const uint8_t **out, int *w, int *h,
                         int *stride);
void fb_decoder_free(fb_decoder_t *d);
```

### Return-code contract (`enum fb_decoder_status`)

| Value | Name                    | Meaning |
| ----- | ----------------------- | ------- |
| 0     | `FB_DECODER_OK`         | Unit accepted / a frame was produced. |
| 1     | `FB_DECODER_AGAIN`      | Success but no output yet (config held for merge, or avcodec `EAGAIN`). Not an error. |
| -1    | `FB_DECODER_ERR_INVALID`| Invalid argument (NULL handle, NULL data with non-zero len). |
| -2    | `FB_DECODER_ERR_OOM`    | Out of memory buffering/growing a packet. |
| -3    | `FB_DECODER_ERR_FATAL`  | Unrecoverable libav failure. |

Negative values are fatal; non-negative values are normal flow.

### Buffer ownership

`fb_decoder_take_rgba` returns a pointer to a decoder-owned RGBA buffer that is
valid ONLY until the next `fb_decoder_feed`, `fb_decoder_take_rgba`, or
`fb_decoder_free` on the same handle. The Rust wrapper MUST copy it into an
owned `DecodedFrame` before the next call; the caller never frees it. The
stride is reported so the Rust side can strip any swscale row padding exactly as
`crates/desktop/src/decode_loop.rs::pack_rgba` already does.

## 4. Threading model

FluxBridge already runs the scrcpy stream on a dedicated worker thread, and
`fluxbridge_core::VideoDecoder` requires `Send` because the decoder is moved
onto that worker. A `fb_decoder_t` handle is NOT thread-safe: it is created,
fed, drained, and freed from that single worker thread only. The Rust wrapper
owns the raw handle exclusively (a private field, never shared, cloned, or
aliased) and is the only path to it, mirroring the `SendScaler` single-owner
`unsafe impl Send` note in `crates/desktop/src/decode_loop.rs` (around lines
149-170). No frame pointer escapes the worker; each frame is copied into an
owned `DecodedFrame` before being handed to the UI channel.

## 5. Feature-gating strategy

A NEW cargo feature `native-decode` selects the native engine. It is:

- OFF by default (`default = []` is unchanged).
- NOT implied by `default` and NOT implied by `gui`.
- The only thing that pulls in the vendored C compile and the FFmpeg link.

With the feature OFF, the default and `gui` builds keep the pure-Rust
`StubDecoder` and have ZERO native / FFmpeg linkage, so the headless Linux host
(which has no FFmpeg/libav, no display, no device) stays green:

- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace`
- `cargo build -p fluxbridge-desktop --features gui`

The native C engine, the `scrcpy-decode-sys` FFI crate, and the `native-decode`
cargo feature are added in later milestones (FEAT-003/FEAT-004); THIS milestone
adds only files under `native/` and `docs/` and wires no build.

Plan for the existing `decode` / `ffmpeg-next` path: it is kept as a fallback
for now (it is pure-Rust FFmpeg bindings and does not need the vendored C), and
`native-decode` takes precedence when enabled. Retiring the `decode` path is
deferred until `native-decode` is validated on Windows CI and by the user;
tracked as a later cleanup, not part of this milestone.

## 6. How it slots behind fluxbridge_core::VideoDecoder

The FFI wrapper (a `NativeScrcpyDecoder` added in FEAT-003) implements the
existing `VideoDecoder` trait (`crates/core/src/traits.rs`):

- `configure(&VideoConfig)` maps codec + dimensions to `fb_decoder_new`.
- `decode(&[u8])` calls `fb_decoder_feed` for the access unit, then drains
  `fb_decoder_take_rgba`, copying each frame into a
  `DecodedFrame{ pixel_format: "rgba", data.len() == width*height*4 }`.
- `flush()` releases and recreates the handle (or drains pending output).

Because the produced `DecodedFrame` already satisfies the Slint render contract
(`pixel_format == "rgba"` AND `data.len() == width*height*4`), the existing
`crates/desktop/src/ui.rs::decoded_frame_to_image` path renders it into the
in-window `SharedPixelBuffer<Rgba8Pixel>` surface with no UI change.
`crates/desktop/src/scrcpy.rs::stream_av_packets` needs only a minimal change:
stop calling `configure()` with empty dimensions for config units and instead
pass the config unit through `decode()` so the merge happens inside the engine.

## 7. Critical correctness note

The config (SPS/PPS) access unit MUST be prepended to the next media packet
(scrcpy `packet_merger`) and sent as ONE `AVPacket`. It MUST NOT be delivered
via a separate `configure()` call with empty dimensions and a standalone
extradata buffer. That old behaviour is the root cause of the unresolved
`send_packet: invalid data found when processing input` error. The native engine
fixes it by running `packet_merger` inside `fb_decoder_feed`.

## 8. Verification matrix (honest)

| Aspect | Where verified | Status in this milestone |
| ------ | -------------- | ------------------------ |
| Default build links with `native-decode` OFF | Headless Linux (this host) | Verified: `cargo build --workspace` |
| Test suite stays green, count unchanged | Headless Linux (this host) | Verified: `cargo test --workspace` |
| Clippy clean | Headless Linux (this host) | Verified: `cargo clippy --workspace` |
| GUI build compiles with no C/FFmpeg | Headless Linux (this host) | Verified: `cargo build -p fluxbridge-desktop --features gui` |
| `packet_merger.{c,h}` compile standalone against libavcodec | By inspection here; real compile on Windows CI | Inspection only this milestone |
| `fb_decoder.h` FFI signatures / ABI | By inspection here | Inspection only |
| Vendored C compiles AND links against FFmpeg | windows-latest CI (LLVM + BtbN FFmpeg 7.1.3 shared) | NOT verifiable headless; CI-only |
| Real pixels render in the Slint window | User's Samsung SM-F946B (Fold5, Android 17) | NOT verifiable here; user confirms |

This milestone never claims that pixels render. It adds reviewable design,
vendored sources, a C API header, and license/attribution files, and changes no
Rust behaviour.

## 9. Licensing and attribution

scrcpy is licensed under the Apache License, Version 2.0, which permits
commercial use. FluxBridge ships, alongside the code and in release artifacts:

- `native/scrcpy-decode/LICENSE`: the full scrcpy Apache-2.0 license text.
- `native/scrcpy-decode/NOTICE`: attribution to Genymobile and scrcpy
  contributors, the list of vendored files, and a statement of the
  include-only modifications (Apache-2.0 section 4).

Bundling these into the release zip and the installer is wired in FEAT-004;
this milestone only lands the files and references them here and in
`native/scrcpy-decode/VENDOR.md`.
