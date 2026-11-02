//! # fluxbridge-core
//!
//! OS-agnostic core of [FluxBridge](https://github.com/Nub4Fun22/FluxBridge), a
//! cross-platform desktop application for smartphone mirroring, remote control
//! and advanced synchronization.
//!
//! This crate contains two things and nothing platform-specific that links to a
//! native library:
//!
//! 1. The [`protocol`] module: the binary wire protocol (packet codec) used on
//!    every transport between the host and a connected device.
//! 2. The [`traits`] module: abstractions that decouple the application logic
//!    from the operating system and from vendor integrations (FFmpeg hardware
//!    decode, ADB, Win32 OLE, Wayland/X11, libimobiledevice). Concrete backends
//!    implement these traits; the [`platform`] module ships compile-ready stub
//!    implementations so the whole workspace builds and tests on any host.
//!
//! See `docs/PROTOCOL.md` and `docs/ARCHITECTURE.md` in the repository for the
//! full specification and blueprint.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod platform;
pub mod protocol;
pub mod traits;

pub use error::{CoreError, CoreResult, ProtocolError};
pub use protocol::{
    DecodeResult, FileChunk, FileMeta, KeyEvent, MouseEvent, Packet, PacketCodec, PacketType,
    Payload, TouchEvent, HEADER_LEN, MAGIC_0, MAGIC_1, PROTOCOL_VERSION,
};
pub use traits::{
    ClipboardImage, ClipboardSync, DecodedFrame, DragDropBridge, DragFile, EncodedFrame, InputSink,
    ReleaseInfo, Updater, VideoCapture, VideoConfig, VideoDecoder,
};
