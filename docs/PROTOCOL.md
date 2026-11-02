# FluxBridge wire protocol

This document specifies the binary packet format implemented in
`crates/core/src/protocol.rs`. The protocol is transport-agnostic: the same
framing is used over any FluxBridge-native socket, TCP/Wi-Fi and any future iOS
bridge.

> **Scope: this is the FluxBridge `0xFB` protocol, not the v1 scrcpy path.** The
> shipped Android v1 mirroring path drives the upstream scrcpy server over adb
> and therefore speaks scrcpy's OWN wire format on those sockets: a one-shot
> video stream header, length-prefixed H.264 access units, and scrcpy control
> messages. That framing is handled in `crates/desktop/src/scrcpy.rs` and is
> deliberately separate from the `0xFB` format below. The two never share a
> socket. The `0xFB` protocol documented here is retained for a future
> FluxBridge-native server and for transport-agnostic input, clipboard and file
> framing; see [`ARCHITECTURE.md`](ARCHITECTURE.md) for how the two fit together.

All multi-byte integers are **big-endian** (network order). Strings are encoded
as a big-endian `u16` byte-length followed by that many UTF-8 bytes. Because the
length prefix is a `u16`, a single string field (for example a `FileMeta` name
or mime) is limited to 65535 bytes; encoding a longer string fails with
`PayloadTooLarge`. This is a protocol-level bound, not just an implementation
detail.

## Packet framing

Every packet begins with an 8-byte fixed header:

| Offset | Size | Field          | Value / type              |
|-------:|-----:|----------------|---------------------------|
| 0      | 1    | magic 0        | `0xFB`                    |
| 1      | 1    | magic 1        | `0x01`                    |
| 2      | 1    | version        | `u8`, currently `1`       |
| 3      | 1    | packet_type    | `u8` (see table below)    |
| 4      | 4    | payload length | `u32` big-endian          |
| 8      | N    | payload        | `length` bytes            |

The header length is 8 bytes (`HEADER_LEN`). Total packet size is
`8 + payload_length`.

### Decoding rules and errors

`PacketCodec::decode` reads exactly one packet from the front of a buffer and
reports how many bytes it consumed, so it can be driven over a growing stream
buffer. It fails with a `ProtocolError`:

- `BadMagic` - bytes 0..2 are not `0xFB 0x01`.
- `UnsupportedVersion` - byte 2 is not the supported version.
- `UnknownPacketType` - byte 3 is not a known type.
- `Truncated { needed, got }` - the buffer does not yet hold the full header or
  the declared payload. Callers should read more bytes and retry.
- `PayloadTooLarge` - on encode, a payload or string exceeds its length field.
- `InvalidUtf8` - a string field was not valid UTF-8.

## Packet types

| Value  | Name            | Payload |
|-------:|-----------------|---------|
| `0x01` | Handshake       | capability exchange (opaque at core layer) |
| `0x02` | VideoConfig     | codec/resolution/extra-data |
| `0x03` | VideoFrame      | one encoded video frame |
| `0x10` | MouseEvent      | see below |
| `0x11` | KeyEvent        | see below |
| `0x12` | TouchEvent      | see below |
| `0x20` | ClipboardText   | UTF-8 clipboard text |
| `0x21` | ClipboardImage  | raw clipboard image |
| `0x30` | FileChunk       | see below |
| `0x31` | FileMeta        | see below |
| `0x40` | DragEnter       | reverse drag entered host |
| `0x41` | DragDrop        | reverse drag dropped on host |
| `0x50` | Battery         | battery / signal status |
| `0x60` | Ack             | acknowledgement |
| `0x61` | Heartbeat       | keep-alive |

## Typed payloads

### MouseEvent (type `0x10`), 8 bytes

| Offset | Size | Field  | Type  | Meaning |
|-------:|-----:|--------|-------|---------|
| 0      | 2    | x      | `u16` | X in device pixels |
| 2      | 2    | y      | `u16` | Y in device pixels |
| 4      | 1    | button | `u8`  | 0 none/move, 1 left, 2 right, 3 middle |
| 5      | 1    | action | `u8`  | 0 move, 1 down, 2 up, 3 scroll |
| 6      | 2    | scroll | `i16` | signed scroll delta |

### KeyEvent (type `0x11`), 11 bytes

| Offset | Size | Field     | Type  | Meaning |
|-------:|-----:|-----------|-------|---------|
| 0      | 4    | keycode   | `u32` | platform-independent key code |
| 4      | 4    | scancode  | `u32` | hardware scan code |
| 8      | 1    | action    | `u8`  | 0 up, 1 down, 2 repeat |
| 9      | 2    | modifiers | `u16` | shift/ctrl/alt/meta bitmask |

### TouchEvent (type `0x12`), 8 bytes

| Offset | Size | Field      | Type  | Meaning |
|-------:|-----:|------------|-------|---------|
| 0      | 1    | pointer_id | `u8`  | multi-touch pointer id |
| 1      | 2    | x          | `u16` | X in device pixels |
| 3      | 2    | y          | `u16` | Y in device pixels |
| 5      | 1    | action     | `u8`  | 0 down, 1 up, 2 move, 3 cancel |
| 6      | 2    | pressure   | `u16` | normalized 0..=65535 |

### FileMeta (type `0x31`), variable length

The offsets below follow the **encode order** in `FileMeta::encode_payload`
(`file_id`, then `size`, then `name`, then `mime`), which is the order on the
wire. This differs from the Rust struct's field-declaration order; the table
tracks the encoder, so keep the two in sync if the encoder is ever reordered.

| Offset | Size | Field   | Type   | Meaning |
|-------:|-----:|---------|--------|---------|
| 0      | 4    | file_id | `u32`  | transfer id |
| 4      | 8    | size    | `u64`  | total file size in bytes |
| 12     | 2+L1 | name    | string | `u16` length + UTF-8 bytes |
| ...    | 2+L2 | mime    | string | `u16` length + UTF-8 bytes |

### FileChunk (type `0x30`), variable length

| Offset | Size | Field   | Type   | Meaning |
|-------:|-----:|---------|--------|---------|
| 0      | 4    | file_id | `u32`  | matches a FileMeta |
| 4      | 4    | seq     | `u32`  | chunk sequence number |
| 8      | 1    | is_last | `u8`   | 0 = more follow, non-zero = final chunk |
| 9      | rest | data    | bytes  | chunk payload (remaining bytes) |

## Example

A `MouseEvent { x: 1280, y: 720, button: 1, action: 2, scroll: -3 }` frames to:

```
FB 01 01 10 00 00 00 08   05 00 02 D0 01 02 FF FD
|header: magic ver type |  |length=8 |  |payload  |
```

where the payload decodes as x=0x0500 (1280), y=0x02D0 (720), button=0x01,
action=0x02, scroll=0xFFFD (-3).
