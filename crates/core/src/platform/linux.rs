//! Linux (and generic Unix) stub implementations of the OS-facing traits.
//!
//! Compiled under `#[cfg(unix)]`, which also covers this Linux CI host so the
//! workspace builds and tests cleanly here.
//!
//! Real implementation plan:
//! * `LinuxDragDropBridge` implements XDnD on X11 and the `wl_data_device`
//!   protocol on Wayland for reverse drag-and-drop.
//! * `LinuxClipboardSync` bridges the X11 `CLIPBOARD` selection / Wayland
//!   `wl_data_source` and `wl_data_offer`.
//! * `LinuxVideoDecoder` wraps FFmpeg with VA-API / VDPAU hardware decode.

use crate::error::{CoreError, CoreResult};
use crate::traits::{
    ClipboardImage, ClipboardSync, DecodedFrame, DragDropBridge, DragFile, VideoConfig,
    VideoDecoder,
};

/// XDnD / Wayland `wl_data_device` reverse drag-and-drop bridge (stub).
#[derive(Debug, Default)]
pub struct LinuxDragDropBridge;

impl DragDropBridge for LinuxDragDropBridge {
    fn begin_reverse_drag(&mut self, _files: &[DragFile]) -> CoreResult<()> {
        Err(CoreError::NotImplemented(
            "LinuxDragDropBridge::begin_reverse_drag (XDnD / wl_data_device)",
        ))
    }

    fn offer_file(&mut self, _file: &DragFile, _bytes: &[u8]) -> CoreResult<Option<String>> {
        Err(CoreError::NotImplemented(
            "LinuxDragDropBridge::offer_file (XDnD / wl_data_device)",
        ))
    }
}

/// X11 / Wayland clipboard synchronization (stub).
#[derive(Debug, Default)]
pub struct LinuxClipboardSync;

impl ClipboardSync for LinuxClipboardSync {
    fn push_text(&mut self, _text: &str) -> CoreResult<()> {
        Err(CoreError::NotImplemented(
            "LinuxClipboardSync::push_text (CLIPBOARD selection / wl_data_source)",
        ))
    }

    fn pull_text(&mut self) -> CoreResult<Option<String>> {
        Ok(None)
    }

    fn push_image(&mut self, _image: &ClipboardImage) -> CoreResult<()> {
        Err(CoreError::NotImplemented(
            "LinuxClipboardSync::push_image (CLIPBOARD selection / wl_data_source)",
        ))
    }

    fn pull_image(&mut self) -> CoreResult<Option<ClipboardImage>> {
        Ok(None)
    }
}

/// FFmpeg + VA-API / VDPAU hardware video decoder (stub).
#[derive(Debug, Default)]
pub struct LinuxVideoDecoder;

impl VideoDecoder for LinuxVideoDecoder {
    fn configure(&mut self, _config: &VideoConfig) -> CoreResult<()> {
        Err(CoreError::NotImplemented(
            "LinuxVideoDecoder::configure (FFmpeg VA-API/VDPAU)",
        ))
    }

    fn decode(&mut self, _encoded: &[u8]) -> CoreResult<DecodedFrame> {
        Err(CoreError::NotImplemented(
            "LinuxVideoDecoder::decode (FFmpeg VA-API/VDPAU)",
        ))
    }

    fn flush(&mut self) -> CoreResult<()> {
        Ok(())
    }
}
