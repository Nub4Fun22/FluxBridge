//! Shared test-only environment serialization for the desktop crate.
//!
//! adb discovery ([`crate::adb`]), the pipeline's offline fallback
//! ([`crate::pipeline`]), and the per-user data dir resolution
//! ([`crate::provision::data_dir`], which reads `HOME` / `XDG_DATA_HOME` on
//! unix and `LOCALAPPDATA` on Windows) all read PROCESS-GLOBAL environment
//! variables. Several tests mutate those same variables to force a
//! deterministic outcome (adb absent, no data dir, ...).
//!
//! Environment variables are shared across the whole process, so every test
//! runs in parallel against the SAME set. If each env-mutating test guarded
//! itself with its own private `Mutex`, those mutexes would not serialize
//! against each other and a reader in one test could observe another test's
//! half-applied mutation. That is a nondeterministic data race, not a logic
//! bug, and it showed up as an occasional single-test failure under
//! `cargo test --workspace`.
//!
//! The fix is ONE process-global lock, [`ENV_LOCK`], that every env-touching
//! test acquires before snapshotting/mutating env and holds until it has
//! restored env. [`with_env_lock`] wraps the save/mutate/restore dance over the
//! full key set any of these tests touch, so no test leaks or depends on a
//! variable another one changed.

use std::ffi::OsString;
use std::sync::Mutex;

/// The single process-global lock all env-mutating desktop tests share, so they
/// are mutually exclusive with each other (and with env-reading tests that go
/// through [`with_env_lock`]).
///
/// Acquisition is lock-poisoning tolerant (`unwrap_or_else(|p| p.into_inner())`)
/// so a panicking test does not cascade failures into every other env test.
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The full set of environment variables any env-mutating desktop test touches.
///
/// Every such test saves AND restores this whole set (mutating whichever subset
/// it needs in between) so the save/restore sets are consistent and no test
/// leaks state. It covers both the adb discovery inputs
/// (`ANDROID_HOME`, `ANDROID_SDK_ROOT`, `PATH`, `HOME`, `LOCALAPPDATA`) and
/// `XDG_DATA_HOME`, which the `directories` crate reads on Linux to resolve the
/// data dir.
pub(crate) const ENV_KEYS: [&str; 6] = [
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    "PATH",
    "HOME",
    "LOCALAPPDATA",
    "XDG_DATA_HOME",
];

/// Run `f` with [`ENV_LOCK`] held and a saved/restored snapshot of [`ENV_KEYS`].
///
/// The lock is acquired BEFORE the snapshot and held until env is restored, so
/// no other env-touching test can observe a half-applied mutation. `f` is free
/// to set/remove any of the keys; the originals are put back on the way out,
/// including on an early return from `f` (the restore runs after `f` returns;
/// a panicking `f` poisons the lock, which the next acquirer tolerates).
pub(crate) fn with_env_lock(f: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let saved: Vec<(&str, Option<OsString>)> = ENV_KEYS
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect();
    f();
    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}
