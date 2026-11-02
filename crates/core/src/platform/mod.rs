//! Platform stub implementations of the OS-facing core traits.
//!
//! These stubs let the whole workspace compile and run on any host, including
//! this headless Linux CI sandbox. Each stub either returns
//! [`CoreError::NotImplemented`](crate::error::CoreError::NotImplemented) or
//! performs a harmless no-op. The real backends (Win32 OLE, Wayland/X11,
//! FFmpeg hardware decode, ADB transport) replace these per platform and are
//! described in the doc comments of each stub.
//!
//! Selection is done with `#[cfg(...)]`:
//! * `#[cfg(windows)]` pulls in [`windows`] stubs.
//! * `#[cfg(unix)]` pulls in [`linux`] stubs (covers the Linux/Wayland/X11 port
//!   and the Linux CI host).

#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub mod linux;

/// The platform-default drag-and-drop bridge for the current OS.
#[cfg(windows)]
pub use windows::WindowsDragDropBridge as PlatformDragDropBridge;

/// The platform-default drag-and-drop bridge for the current OS.
#[cfg(unix)]
pub use linux::LinuxDragDropBridge as PlatformDragDropBridge;

/// The platform-default clipboard sync for the current OS.
#[cfg(windows)]
pub use windows::WindowsClipboardSync as PlatformClipboardSync;

/// The platform-default clipboard sync for the current OS.
#[cfg(unix)]
pub use linux::LinuxClipboardSync as PlatformClipboardSync;

/// The platform-default video decoder for the current OS.
#[cfg(windows)]
pub use windows::WindowsVideoDecoder as PlatformVideoDecoder;

/// The platform-default video decoder for the current OS.
#[cfg(unix)]
pub use linux::LinuxVideoDecoder as PlatformVideoDecoder;

#[cfg(test)]
mod tests {
    use super::PlatformVideoDecoder;
    use crate::traits::{VideoConfig, VideoDecoder};

    #[test]
    fn platform_decoder_is_constructible() {
        // The platform alias must resolve and the stub must be instantiable on
        // whatever host runs the tests.
        let mut dec = PlatformVideoDecoder;
        let cfg = VideoConfig {
            codec: "h264".to_string(),
            width: 1920,
            height: 1080,
            extra_data: vec![],
        };
        // The stub reports NotImplemented; we only assert it does not panic.
        let _ = dec.configure(&cfg);
        let _ = dec.decode(&[0u8; 4]);
    }
}
