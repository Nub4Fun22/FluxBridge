//! Windows stub implementations of the OS-facing traits.
//!
//! Compiled only under `#[cfg(windows)]`. On this Linux CI host the module is
//! not built, but it is kept warning-free and object-safe so a real Windows
//! build compiles unchanged.
//!
//! Real implementation plan:
//! * `WindowsDragDropBridge` wraps OLE `IDataObject` / `IDropSource` and
//!   `DoDragDrop` for reverse drag-and-drop, materializing device files to a
//!   temp path exposed as `CF_HDROP`.
//! * `WindowsClipboardSync` uses the Win32 clipboard API (`OpenClipboard`,
//!   `SetClipboardData` with `CF_UNICODETEXT` / `CF_DIBV5`).
//! * `WindowsVideoDecoder` wraps FFmpeg with the DXVA2 / D3D11VA hardware
//!   decode path.

use crate::error::{CoreError, CoreResult};
use crate::traits::{
    ClipboardImage, ClipboardSync, DecodedFrame, DragDropBridge, DragFile, VideoConfig,
    VideoDecoder,
};

/// Windows OLE-based reverse drag-and-drop bridge (stub).
#[derive(Debug, Default)]
pub struct WindowsDragDropBridge;

impl DragDropBridge for WindowsDragDropBridge {
    fn begin_reverse_drag(&mut self, _files: &[DragFile]) -> CoreResult<()> {
        Err(CoreError::NotImplemented(
            "WindowsDragDropBridge::begin_reverse_drag (OLE IDropSource)",
        ))
    }

    fn offer_file(&mut self, _file: &DragFile, _bytes: &[u8]) -> CoreResult<Option<String>> {
        Err(CoreError::NotImplemented(
            "WindowsDragDropBridge::offer_file (OLE IDataObject)",
        ))
    }
}

/// Win32 clipboard synchronization (stub).
#[derive(Debug, Default)]
pub struct WindowsClipboardSync;

impl ClipboardSync for WindowsClipboardSync {
    fn push_text(&mut self, _text: &str) -> CoreResult<()> {
        Err(CoreError::NotImplemented(
            "WindowsClipboardSync::push_text (CF_UNICODETEXT)",
        ))
    }

    fn pull_text(&mut self) -> CoreResult<Option<String>> {
        Ok(None)
    }

    fn push_image(&mut self, _image: &ClipboardImage) -> CoreResult<()> {
        Err(CoreError::NotImplemented(
            "WindowsClipboardSync::push_image (CF_DIBV5)",
        ))
    }

    fn pull_image(&mut self) -> CoreResult<Option<ClipboardImage>> {
        Ok(None)
    }
}

/// FFmpeg + DXVA2 / D3D11VA hardware video decoder (stub).
#[derive(Debug, Default)]
pub struct WindowsVideoDecoder;

impl VideoDecoder for WindowsVideoDecoder {
    fn configure(&mut self, _config: &VideoConfig) -> CoreResult<()> {
        Err(CoreError::NotImplemented(
            "WindowsVideoDecoder::configure (FFmpeg DXVA2/D3D11VA)",
        ))
    }

    fn decode(&mut self, _encoded: &[u8]) -> CoreResult<DecodedFrame> {
        Err(CoreError::NotImplemented(
            "WindowsVideoDecoder::decode (FFmpeg DXVA2/D3D11VA)",
        ))
    }

    fn flush(&mut self) -> CoreResult<()> {
        Ok(())
    }
}
