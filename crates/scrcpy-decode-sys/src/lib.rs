//! Raw FFI bindings to the FluxBridge native decode engine (`fb_decoder`).
//!
//! The engine is the vendored scrcpy `packet_merger` plus a FluxBridge
//! reimplementation of scrcpy's FFmpeg send/receive + swscale-to-RGBA sequence
//! (see `native/scrcpy-decode/fb_decoder.h` for the full C API contract and
//! `docs/NATIVE_DECODE.md` for the design).
//!
//! This crate is intentionally thin: it exposes ONLY the `extern "C"`
//! declarations and the status-code constants that mirror `fb_decoder.h`. The
//! actual C compilation and the FFmpeg link happen in `build.rs` and ONLY when
//! the `build-native` cargo feature is enabled. With the feature OFF (the
//! default, and what the headless Linux host uses) nothing is compiled or
//! linked: the declarations are still present so dependent crates type-check,
//! but calling them would fail to link, which is why the safe wrapper
//! (`NativeScrcpyDecoder` in the desktop crate) is itself gated behind the
//! desktop `native-decode` feature that turns this crate's `build-native` on.
//!
//! # Safety
//!
//! Every function here is `unsafe` to call. The invariants (opaque handle
//! lifetime, single-thread ownership, borrowed-until-next-call RGBA buffer) are
//! documented on each item and enforced by the safe wrapper.

#![no_std]
#![allow(non_camel_case_types)]

use core::ffi::c_int;

/// Status codes returned by [`fb_decoder_feed`] and [`fb_decoder_take_rgba`],
/// mirroring `enum fb_decoder_status` in `fb_decoder.h`. Negative values are
/// fatal; non-negative values are normal, recoverable flow.
pub mod status {
    use super::c_int;

    /// A unit was accepted / a frame was produced.
    pub const FB_DECODER_OK: c_int = 0;
    /// The call succeeded but no output is available yet (config held for merge,
    /// or avcodec returned `EAGAIN`). Not an error.
    pub const FB_DECODER_AGAIN: c_int = 1;
    /// Invalid argument (NULL handle, NULL data with non-zero len, etc.).
    pub const FB_DECODER_ERR_INVALID: c_int = -1;
    /// Out-of-memory while buffering the config packet or growing a packet.
    pub const FB_DECODER_ERR_OOM: c_int = -2;
    /// A libav call failed in a way the stream cannot recover from.
    pub const FB_DECODER_ERR_FATAL: c_int = -3;
}

/// Opaque decoder handle. Allocated by [`fb_decoder_new`], released by
/// [`fb_decoder_free`]. Never constructed or dereferenced on the Rust side.
///
/// This is the usual opaque-FFI-struct pattern: a zero-sized, uninhabited-like
/// type that only ever appears behind a pointer.
#[repr(C)]
pub struct fb_decoder {
    _private: [u8; 0],
}

extern "C" {
    /// Create a decoder for the given stream. See `fb_decoder.h`.
    ///
    /// Returns a new handle or NULL on allocation / codec-open failure. The
    /// caller owns the handle and must release it with [`fb_decoder_free`].
    ///
    /// # Safety
    ///
    /// The returned pointer, if non-null, must be passed only to the other
    /// `fb_decoder_*` functions and freed exactly once.
    pub fn fb_decoder_new(codec_fourcc: u32, width: u32, height: u32) -> *mut fb_decoder;

    /// Feed exactly one scrcpy access unit (one demuxed payload).
    ///
    /// Runs the vendored packet merger internally: a config unit
    /// (`is_config != 0`) is remembered and prepended to the next media packet.
    /// See `fb_decoder.h` for the full contract and the return-code meanings.
    ///
    /// # Safety
    ///
    /// `d` must be a live handle from [`fb_decoder_new`]. When `len > 0`,
    /// `data` must point to at least `len` readable bytes. The handle must not
    /// be used from another thread concurrently.
    pub fn fb_decoder_feed(
        d: *mut fb_decoder,
        data: *const u8,
        len: usize,
        is_config: c_int,
        is_keyframe: c_int,
    ) -> c_int;

    /// Pull the next decoded frame, converted to tightly packed RGBA.
    ///
    /// On [`status::FB_DECODER_OK`] the `out` pointer receives a decoder-owned
    /// buffer valid only until the next `fb_decoder_*` call on `d`; the caller
    /// must copy it out and must NOT free it. See `fb_decoder.h`.
    ///
    /// # Safety
    ///
    /// `d` must be a live handle. `out`, `w`, `h`, and `stride` must be valid,
    /// writable pointers. The handle must not be used from another thread
    /// concurrently.
    pub fn fb_decoder_take_rgba(
        d: *mut fb_decoder,
        out: *mut *const u8,
        w: *mut c_int,
        h: *mut c_int,
        stride: *mut c_int,
    ) -> c_int;

    /// Release a decoder handle and all associated libav/swscale resources.
    /// Safe to call with NULL.
    ///
    /// # Safety
    ///
    /// `d` must be a handle from [`fb_decoder_new`] that has not already been
    /// freed, or NULL. After this call the pointer is dangling and must not be
    /// used again.
    pub fn fb_decoder_free(d: *mut fb_decoder);
}
