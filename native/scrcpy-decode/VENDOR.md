# Vendored scrcpy sources

This note records exactly what was vendored into FluxBridge's native decode
engine, from where, and at which upstream revision. See `NOTICE` for the
Apache-2.0 attribution and statement of modifications, and `../../docs/NATIVE_DECODE.md`
for the full design.

## Upstream

- Project: scrcpy (https://github.com/Genymobile/scrcpy)
- Tag: `v5.0.1`
- Tag commit sha: `a0c27ff382c411af1301dcc1cd8205c9aeb81996`
- License: Apache License, Version 2.0 (see `LICENSE`)

## Vendored verbatim (with include-only trim)

| FluxBridge path                 | Upstream path               | Last-touch commit sha                      | Modification |
| ------------------------------- | --------------------------- | ------------------------------------------ | ------------ |
| `vendor/packet_merger.h`        | `app/src/packet_merger.h`   | `af15c72f9caef4f829d337c35701d8bc00a58989` | `#include "common.h"` replaced with `<stddef.h>` |
| `vendor/packet_merger.c`        | `app/src/packet_merger.c`   | `af15c72f9caef4f829d337c35701d8bc00a58989` | `#include "util/log.h"` replaced with a local `LOG_OOM()` macro |

The merge algorithm in these files is UNCHANGED from upstream. They implement
the SPS/PPS handling that FluxBridge's old pure-Rust decode path got wrong: a
config packet (`pts == AV_NOPTS_VALUE`) is remembered, then its bytes are
prepended to the next media packet so the decoder receives one
`[SPS/PPS][keyframe]` Annex-B `AVPacket`.

## Reimplemented from reference (NOT vendored)

`fb_decoder.h` and its implementation reimplement, rather than copy, scrcpy's
decode sequence. Reference files:

- `app/src/decoder.c` (`sc_decoder_process_packet`): the
  `avcodec_send_packet` + loop `avcodec_receive_frame` sequence and the
  `AV_CODEC_FLAG_LOW_DELAY` flag.
- `app/src/demuxer.c`: the 12-byte frame-header framing (8-byte pts/flags with
  bit 62 = config, bit 61 = keyframe, bit 63 / MSB = session) + 4-byte length,
  and the `AVCodecParameters` setup (codec id, width/height, `AV_PIX_FMT_YUV420P`).

These were not vendored verbatim because scrcpy's `decoder.c` / `demuxer.c` are
entangled with scrcpy's own util layer (hardware-decode selection, the
frame-source / packet-sink traits, `util/vecdeque`, `util/thread` mutex/cond,
the stream session, `container_of`, and custom logging) and pull in far more
than the decode core.

## How to re-fetch

```sh
gh api repos/Genymobile/scrcpy/contents/app/src/packet_merger.h?ref=v5.0.1 --jq '.content' | base64 -d
gh api repos/Genymobile/scrcpy/contents/app/src/packet_merger.c?ref=v5.0.1 --jq '.content' | base64 -d
gh api repos/Genymobile/scrcpy/contents/LICENSE?ref=v5.0.1 --jq '.content' | base64 -d
```
