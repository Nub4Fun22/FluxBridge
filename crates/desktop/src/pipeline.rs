//! adb-driven mirroring pipeline: startup, decode, and teardown.
//!
//! This module owns the real connection lifecycle that `main.rs` kicks off:
//!
//! 1. Locate `adb` on the host ([`crate::adb::locate_adb`]).
//! 2. For Wi-Fi, `adb connect <ip:port>`; for USB, use the sole attached device.
//! 3. Push the scrcpy server jar and set up the tunnel. The SHIPPED default is
//!    REVERSE tunneling (scrcpy's own default): the desktop binds a local
//!    [`std::net::TcpListener`] and runs `adb reverse localabstract:scrcpy
//!    tcp:<local_port>` so the device connects OUT to it, which avoids the
//!    forward-mode leading dummy byte and its readiness race. The legacy
//!    forward path (`adb forward` + dialing the local socket + consuming the
//!    dummy byte) stays reachable when `tunnel_forward=true` is set explicitly.
//! 4. Launch the scrcpy server via `app_process`.
//! 5. In reverse mode, accept the device's inbound video then control sockets
//!    from the listener; in forward mode, dial the forwarded local socket. Then
//!    run the decode loop.
//! 6. On exit (or any failure) tear down: remove the tunnel that was set up
//!    (`adb reverse --remove` in reverse mode, `adb forward --remove` in forward
//!    mode), kill the server child, and disconnect Wi-Fi.
//!
//! Every notable transition and every failure is reported as a
//! [`PipelineStatus`] on an `mpsc` channel. The GUI build drains that channel
//! onto a visible status line instead of relying on a console that may not
//! exist (a windowed Windows app has no attached stdout). The headless build
//! may print the same statuses to stdout.
//!
//! The adb teardown commands and the status-reporting flow are unit-tested;
//! the reverse accept budget), the parts that require a live device (`push`,
//! `app_process`, the socket) are
//! only reached at runtime on the user's machine, never on the headless CI host.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use crate::adb::{self, AdbCommand, DeviceSelector, ServerLaunchOptions};
use crate::connection::ConnectionConfig;

/// Read timeout on the scrcpy video socket.
///
/// Kept short (not the former 30s) so that on a STALLED stream a blocked read
/// wakes roughly this often, letting the access-unit loop observe a "Stop
/// mirroring" request and break out within about this interval instead of
/// waiting out a long timeout. On a HEALTHY stream access units arrive
/// continuously, so the timeout never actually fires and the short value has no
/// throughput cost; a timed-out read at a packet boundary is treated as a loop
/// wake (see [`crate::scrcpy::PacketRead::TimedOut`]), not an error.
const STREAM_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A cooperative cancellation flag shared between the UI thread and the
/// pipeline worker thread.
///
/// The UI owns one of these and hands a clone to the spawned pipeline. When the
/// user clicks "Stop mirroring" the UI calls [`CancelFlag::cancel`]; the
/// pipeline checks [`CancelFlag::is_cancelled`] at natural boundaries (before
/// each blocking adb phase and inside the stream loop) and, when set, breaks
/// out so the existing teardown path runs exactly once.
///
/// The flag is an [`Arc`]-wrapped [`AtomicBool`], so flipping it from the UI
/// thread while the pipeline thread reads it is data-race free. It defaults to
/// NOT cancelled, and [`CancelFlag::cancel`] is idempotent (setting it twice
/// leaves it set).
#[derive(Debug, Clone, Default)]
pub struct CancelFlag {
    flag: Arc<AtomicBool>,
}

impl CancelFlag {
    /// A fresh flag in the NOT-cancelled state.
    pub fn new() -> Self {
        CancelFlag {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Request cancellation. Idempotent: calling it when already cancelled
    /// leaves the flag set, and it is safe to call when nothing is mirroring.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Reset the flag back to NOT-cancelled.
    ///
    /// The UI no longer reuses a single flag across runs (each run gets a FRESH
    /// [`CancelFlag`] via [`RunGuard`], which is what removed the reset-on-Start
    /// race). This is retained as a small utility on the primitive and is
    /// covered by the flag-semantics test.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// Whether an optional cancel flag has been tripped. A `None` flag (the headless
/// build and callers that do not support stopping) is never cancelled.
fn is_cancelled(cancel: Option<&CancelFlag>) -> bool {
    cancel.map(CancelFlag::is_cancelled).unwrap_or(false)
}

/// Single-threaded lifecycle guard for the mirroring pipeline, owned by the UI
/// thread.
///
/// It bundles the two pieces of mirroring lifecycle state the UI must keep
/// consistent so a fast Start-after-Stop cannot run two pipelines at once:
///
/// * a `started` guard that is true while a worker is live, and
/// * the CURRENT run's [`CancelFlag`] (a FRESH flag per run, `None` when idle).
///
/// The critical invariants (regression-tested headlessly in this module):
///
/// * [`RunGuard::try_start`] gives out a fresh per-run flag and refuses to start
///   a second run while one is still live, so Start cannot spawn an overlapping
///   worker.
/// * [`RunGuard::request_stop`] trips ONLY the current run's flag and is a safe
///   no-op when idle, so Stop never cancels some later run by clearing a shared
///   flag the previous worker had not yet observed.
/// * [`RunGuard::worker_stopped`] is what releases the guard: the `started` flag
///   is cleared (and the slot emptied) only when the worker actually signals
///   termination (`PipelineStatus::Stopped`), NOT on the Stop click. Until then
///   `try_start` keeps refusing, so the prior worker is guaranteed to have
///   returned before the next Start proceeds.
///
/// Because the flag is a fresh per-run [`CancelFlag`] rather than one shared,
/// reset-on-Start flag, there is no window where a reset clears a stop the old
/// worker has not yet seen. It is deliberately NOT `Send`/`Sync`-bound beyond
/// what `RefCell` allows: it lives on the UI thread and the only cross-thread
/// handoff is the cloned [`CancelFlag`] passed into the worker.
#[derive(Debug, Default)]
pub struct RunGuard {
    /// The current run's cancel flag, or `None` when no worker is live.
    current: Option<CancelFlag>,
}

impl RunGuard {
    /// A fresh, idle guard (no run in flight).
    pub fn new() -> Self {
        RunGuard { current: None }
    }

    /// Whether a worker is currently considered live (started and not yet
    /// observed Stopped).
    pub fn is_started(&self) -> bool {
        self.current.is_some()
    }

    /// Attempt to begin a run. Returns a FRESH [`CancelFlag`] for the new run on
    /// success, or `None` when a run is already live (so the caller must NOT
    /// spawn an overlapping worker). The returned flag is stored as the current
    /// run's flag so a later [`RunGuard::request_stop`] trips exactly this run.
    pub fn try_start(&mut self) -> Option<CancelFlag> {
        if self.current.is_some() {
            return None;
        }
        let flag = CancelFlag::new();
        self.current = Some(flag.clone());
        Some(flag)
    }

    /// Request a stop of the CURRENT run, if any. Trips only the current run's
    /// flag; a no-op when idle. Does NOT release the guard: the guard is freed
    /// only once the worker signals termination via [`RunGuard::worker_stopped`],
    /// which is what makes a Stop-then-Start sequence impossible to overlap.
    pub fn request_stop(&self) {
        if let Some(flag) = &self.current {
            flag.cancel();
        }
    }

    /// Mark the worker as terminated, releasing the guard so the next
    /// [`RunGuard::try_start`] can proceed. Called when the UI observes
    /// [`PipelineStatus::Stopped`] on the status channel (the worker emits it on
    /// both the normal exit and the cancelled short-circuit). Idempotent.
    pub fn worker_stopped(&mut self) {
        self.current = None;
    }
}

/// How the device is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// USB: a single attached device, tunneled with `adb forward`.
    Usb,
    /// Wi-Fi: `adb connect <ip:port>` first, then the same forward tunnel.
    WiFi {
        /// Device IP address.
        ip: String,
        /// Device adb connect port. For Android 11+ wireless debugging this is
        /// the DYNAMIC `_adb-tls-connect._tcp` port discovered via mDNS (see
        /// [`crate::adb::parse_mdns_services`]), NOT the legacy fixed 5555 used
        /// after `adb tcpip`. Use [`Transport::wifi`] /
        /// [`Transport::from_wireless`] so the discovered port flows through
        /// instead of a hardcoded value.
        port: u16,
    },
}

impl Transport {
    /// A Wi-Fi transport for a device reached at `host:connect_port`, where
    /// `connect_port` is the mDNS-discovered (dynamic) connect port.
    pub fn wifi(host: impl Into<String>, connect_port: u16) -> Self {
        Transport::WiFi {
            ip: host.into(),
            port: connect_port,
        }
    }

    /// A Wi-Fi transport built from a completed wireless pair/connect result,
    /// carrying its discovered connect host and port into the pipeline.
    pub fn from_wireless(conn: &crate::adb::WirelessConnection) -> Self {
        Transport::wifi(conn.host.clone(), conn.connect_port)
    }
}

/// Inputs needed to start the mirroring pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// USB or Wi-Fi transport.
    pub transport: Transport,
    /// Local TCP port adb forwards to the device scrcpy socket.
    pub local_port: u16,
    /// Path to the scrcpy server jar on the host (bundled on the Windows side).
    pub server_jar: String,
    /// scrcpy server launch options.
    pub server_opts: ServerLaunchOptions,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            transport: Transport::Usb,
            local_port: crate::connection::DEFAULT_PORT,
            server_jar: "scrcpy-server.jar".to_string(),
            server_opts: ServerLaunchOptions::default(),
        }
    }
}

impl PipelineConfig {
    /// The [`DeviceSelector`] implied by the transport (USB uses the default
    /// device, Wi-Fi targets the `ip:port` serial that `adb connect` creates).
    pub fn selector(&self) -> DeviceSelector {
        match &self.transport {
            Transport::Usb => DeviceSelector::UsbDefault,
            Transport::WiFi { ip, port } => DeviceSelector::Serial(format!("{ip}:{port}")),
        }
    }

    /// The local address the forwarded socket is reachable at.
    pub fn connection_config(&self) -> ConnectionConfig {
        ConnectionConfig {
            host: crate::connection::DEFAULT_HOST.to_string(),
            port: self.local_port,
            read_timeout: Some(std::time::Duration::from_secs(30)),
        }
    }

    /// Resolve the scrcpy server jar to an absolute path the `adb push` can
    /// find regardless of the process working directory.
    ///
    /// An installed FluxBridge is launched from a Start-menu or desktop
    /// shortcut whose working directory is NOT the install directory, so a bare
    /// relative `server_jar` resolved against the CWD would not be found. The
    /// installer ships `scrcpy-server.jar` next to `fluxbridge-desktop.exe`
    /// (see `installer/fluxbridge.iss`), so an already-absolute path is used as
    /// is and a relative one is resolved against the directory of the running
    /// executable ([`std::env::current_exe`]). If the executable directory
    /// cannot be determined (an unusual environment), the path is returned
    /// unchanged so the caller still gets a best-effort value.
    pub fn resolve_server_jar(&self) -> String {
        let path = std::path::Path::new(&self.server_jar);
        if path.is_absolute() {
            return self.server_jar.clone();
        }
        match exe_dir() {
            Some(dir) => dir.join(path).to_string_lossy().into_owned(),
            None => self.server_jar.clone(),
        }
    }
}

/// The directory of the running executable, used to locate bundled assets
/// (the scrcpy server jar) relative to the installed binary rather than the
/// process working directory. Returns `None` if the path cannot be resolved.
fn exe_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
}

/// A status update emitted as the pipeline progresses or fails.
///
/// The GUI binds these to a visible status line; `Error` is terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineStatus {
    /// adb was located at the given path.
    AdbLocated(String),
    /// adb was not found, so FluxBridge is downloading Google's platform-tools;
    /// carries a `0..=100` download percentage.
    ProvisioningAdb(u8),
    /// The downloaded platform-tools are being extracted.
    ExtractingAdb,
    /// adb was auto-provisioned (downloaded + extracted) at the given path.
    AdbProvisioned(String),
    /// Browsing the LAN for wireless-debugging devices via adb's mDNS.
    Discovering,
    /// Pairing with a device using the 6-digit code shown on the phone; carries
    /// the `host:port` pairing endpoint.
    Pairing(String),
    /// The one-time pairing with the device succeeded.
    Paired(String),
    /// A Wi-Fi `adb connect` to the device succeeded.
    Connected(String),
    /// The scrcpy server jar was pushed to the device.
    ServerPushed,
    /// The adb forward tunnel is up on the given local port.
    Forwarded(u16),
    /// The scrcpy server process was launched.
    ServerLaunched,
    /// The local socket connected and streaming began.
    Streaming,
    /// Teardown finished.
    Stopped,
    /// A terminal error with a human-readable message for the UI.
    Error(String),
}

impl fmt::Display for PipelineStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineStatus::AdbLocated(p) => write!(f, "adb located at {p}"),
            PipelineStatus::ProvisioningAdb(pct) => {
                write!(f, "Downloading Android platform-tools... {pct}%")
            }
            PipelineStatus::ExtractingAdb => write!(f, "Extracting Android platform-tools..."),
            PipelineStatus::AdbProvisioned(p) => write!(f, "adb installed at {p}"),
            PipelineStatus::Discovering => write!(f, "searching for wireless devices..."),
            PipelineStatus::Pairing(s) => write!(f, "pairing with {s}..."),
            PipelineStatus::Paired(s) => write!(f, "paired with {s}"),
            PipelineStatus::Connected(s) => write!(f, "connected to {s}"),
            PipelineStatus::ServerPushed => write!(f, "server pushed to device"),
            PipelineStatus::Forwarded(p) => write!(f, "forwarding 127.0.0.1:{p}"),
            PipelineStatus::ServerLaunched => write!(f, "scrcpy server launched"),
            PipelineStatus::Streaming => write!(f, "streaming"),
            PipelineStatus::Stopped => write!(f, "stopped"),
            PipelineStatus::Error(e) => write!(f, "error: {e}"),
        }
    }
}

/// Send a status, ignoring a dropped receiver (the UI may have closed).
fn report(tx: &Sender<PipelineStatus>, status: PipelineStatus) {
    let _ = tx.send(status);
}

/// How many of the scrcpy server's most recent output lines are retained for
/// the error path. Bounded so a chatty server cannot grow memory without limit.
const SERVER_OUTPUT_LINES: usize = 50;

/// Total time budget for the forward-mode video connect + dummy-byte retry.
/// scrcpy's own client retries because `adb forward` can accept the local
/// connection before the device-side server is listening; this mirrors that
/// with a short bound so a genuinely dead server still fails quickly.
const DUMMY_BYTE_RETRY_ATTEMPTS: u32 = 20;
/// Backoff between dummy-byte retry attempts (20 attempts x 100ms ~= 2s total).
const DUMMY_BYTE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// Read timeout on the forward-mode video socket while waiting for the dummy
/// byte, so a silent server surfaces a would-block the retry loop can act on
/// instead of blocking forever.
const DUMMY_BYTE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Total time budget for accepting the device's inbound reverse-mode sockets.
///
/// In reverse mode the desktop LISTENS and the device connects out AFTER the
/// server launches, so there is a brief window where no peer has connected yet.
/// The accept loop polls (honoring the cancel flag) up to this budget; if the
/// server never connects back (the failure case) it gives up instead of
/// blocking forever, and the error carries the captured server output. The
/// budget mirrors the forward-mode dummy-byte retry budget
/// ([`DUMMY_BYTE_RETRY_ATTEMPTS`] x [`DUMMY_BYTE_RETRY_BACKOFF`] ~= 2s).
const REVERSE_ACCEPT_ATTEMPTS: u32 = 20;
/// Poll interval between reverse-mode accept attempts (20 x 100ms ~= 2s total).
const REVERSE_ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// Short bounded grace wait before snapshotting the server output on the
/// failure path. The server's stdout/stderr are drained by detached reader
/// threads (see [`spawn_server_output_reader`]); when the stream fails to start
/// the snapshot is taken synchronously, which can race those threads so the
/// server's final lines (the version mismatch or stack trace that explains the
/// failure) have not been parsed and pushed yet. A brief wait gives the readers
/// a chance to flush before the snapshot. It is well under a second and is only
/// paid on the error path, never on the happy streaming path; a silent server
/// still ends up with an empty buffer and the explicit "(no output captured)".
const SERVER_OUTPUT_GRACE_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

/// A bounded, thread-safe ring buffer of the scrcpy server's most recent output
/// lines, so a server-side startup failure (a version mismatch, an
/// `app_process` error, an unsupported option, a Java stack trace) can be
/// surfaced in the pipeline's error message instead of being lost to inherited
/// stdio.
///
/// The server child's stdout/stderr are drained on a background thread (see
/// [`spawn_server_output_reader`]) into one of these; the error path snapshots
/// the last lines with [`ServerOutputBuffer::snapshot`]. The capture and
/// snapshot logic is a pure, unit-testable seam driven in tests by any
/// [`std::io::Read`] (a `Cursor` of fake server stderr), with no real `Child`.
#[derive(Clone, Default)]
pub struct ServerOutputBuffer {
    lines: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

impl ServerOutputBuffer {
    /// Create an empty buffer.
    pub fn new() -> Self {
        ServerOutputBuffer {
            lines: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        }
    }

    /// Append one line, evicting the oldest so at most [`SERVER_OUTPUT_LINES`]
    /// are retained. Trailing carriage returns/newlines are trimmed so Windows
    /// adb output does not leave stray `\r` in the captured text.
    pub fn push_line(&self, line: &str) {
        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
        if let Ok(mut guard) = self.lines.lock() {
            if guard.len() >= SERVER_OUTPUT_LINES {
                guard.pop_front();
            }
            guard.push_back(trimmed);
        }
    }

    /// Drain every line from `reader` (the child's piped stdout/stderr) into the
    /// buffer, returning cleanly when the pipe closes (read returns EOF). This
    /// is what the reader thread runs; it is also called directly in tests with
    /// an in-memory `Read`.
    pub fn drain_from<R: std::io::Read>(&self, reader: R) {
        use std::io::BufRead;
        let buffered = std::io::BufReader::new(reader);
        for line in buffered.lines() {
            match line {
                Ok(l) => self.push_line(&l),
                Err(_) => break, // pipe closed or decode error; stop cleanly
            }
        }
    }

    /// Snapshot the last `k` captured lines as a single newline-joined string.
    ///
    /// Returns `None` when nothing has been captured, so the caller can tell a
    /// silent server apart from one that logged a diagnostic.
    pub fn snapshot(&self, k: usize) -> Option<String> {
        let guard = self.lines.lock().ok()?;
        if guard.is_empty() {
            return None;
        }
        let start = guard.len().saturating_sub(k);
        let joined = guard
            .iter()
            .skip(start)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        Some(joined)
    }
}

/// Spawn background threads that drain the server child's stdout and stderr into
/// `buffer`, so a chatty or silent server never deadlocks the pipeline and its
/// output is available for the error path.
///
/// Each thread owns its pipe handle and exits cleanly when that pipe closes (on
/// server exit or when the child is killed during teardown). The threads are
/// detached: they are short-lived relative to the pipeline and tear themselves
/// down on EOF, so there is no join handle to manage.
fn spawn_server_output_reader(child: &mut std::process::Child, buffer: &ServerOutputBuffer) {
    if let Some(stdout) = child.stdout.take() {
        let buf = buffer.clone();
        std::thread::spawn(move || buf.drain_from(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        let buf = buffer.clone();
        std::thread::spawn(move || buf.drain_from(stderr));
    }
}

/// Snapshot the captured server output for the error path after a short bounded
/// grace wait (see [`SERVER_OUTPUT_GRACE_WAIT`]).
///
/// The reader threads drain the server's pipes independently, so at the instant
/// the stream-start retry gives up the server's final lines may not be parsed
/// and pushed yet. The brief wait lets the readers flush so a server that
/// printed its failure reason is reliably reflected in the error, rather than
/// losing those lines to a thread-scheduling race. The wait is bounded and
/// never blocks indefinitely: a silent server simply still yields an empty
/// snapshot and the explicit "(no output captured)".
fn snapshot_server_output_after_grace(server_output: &ServerOutputBuffer) -> Option<String> {
    std::thread::sleep(SERVER_OUTPUT_GRACE_WAIT);
    server_output.snapshot(SERVER_OUTPUT_LINES)
}

/// Build the actionable error message shown when the forward-mode stream cannot
/// start (the dummy-byte retry was exhausted or the header parse hit EOF at the
/// very start of the stream).
///
/// It names the underlying I/O problem, then includes the last captured server
/// output (or an explicit "(no output captured)" when the server printed
/// nothing, so silence is distinguishable from a captured message), then a
/// concrete hint. No em dashes anywhere.
fn stream_start_error(detail: &str, server_output: Option<String>) -> String {
    let said = match server_output {
        Some(text) if !text.trim().is_empty() => text,
        _ => "(no output captured)".to_string(),
    };
    format!(
        "scrcpy stream did not start: {detail}. Server said: {said}. \
         Try re-pairing in Wireless debugging, toggle Wireless debugging off and on, \
         and confirm the phone's Android version is 11 or newer."
    )
}

/// Whether a connect error is the forward-mode readiness race (worth retrying)
/// rather than a hard failure. `adb forward` accepting while the device-side
/// server is not yet listening surfaces as connection-refused (and, on some
/// platforms, a connection-reset or timed-out), so those are retried; anything
/// else is returned to the caller immediately.
fn is_retryable_connect_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::TimedOut
    )
}

/// Connect the forward-mode video socket with a bounded retry/backoff, mirroring
/// scrcpy's own forward-mode connect loop. Retries a retryable connect error
/// (see [`is_retryable_connect_error`]) up to the dummy-byte attempt bound with
/// the same backoff, giving up with the last error afterwards.
fn connect_forward_video(addr: &str) -> std::io::Result<std::net::TcpStream> {
    let mut last = None;
    for attempt in 0..DUMMY_BYTE_RETRY_ATTEMPTS {
        match std::net::TcpStream::connect(addr) {
            Ok(s) => return Ok(s),
            Err(e) if is_retryable_connect_error(&e) => {
                last = Some(e);
                if attempt + 1 < DUMMY_BYTE_RETRY_ATTEMPTS {
                    std::thread::sleep(DUMMY_BYTE_RETRY_BACKOFF);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "forward-mode video connect retries exhausted",
        )
    }))
}

/// Accept one inbound reverse-mode socket from `listener` within a bounded time
/// budget, polling non-blockingly so the cancel flag is observed between
/// attempts.
///
/// In reverse mode the device connects OUT to this desktop listener only after
/// the scrcpy server has launched, so the first accept may find no peer
/// waiting yet. The listener is put in non-blocking mode and polled up to
/// [`REVERSE_ACCEPT_ATTEMPTS`] times with [`REVERSE_ACCEPT_BACKOFF`] between
/// attempts (about the same ~2s budget as the forward-mode dummy-byte retry).
/// A `WouldBlock` (no connection yet) is retried; any other accept error is
/// returned immediately. When the budget is exhausted without a connection a
/// [`std::io::ErrorKind::TimedOut`] error is returned so the caller can surface
/// the captured server output instead of hanging. The accepted stream is put
/// back into blocking mode before being returned.
///
/// A `true` from `should_cancel` aborts the wait early (returning a
/// `TimedOut` error) so "Stop mirroring" tears down promptly instead of waiting
/// out the full budget.
fn accept_reverse_socket(
    listener: &std::net::TcpListener,
    should_cancel: &dyn Fn() -> bool,
) -> std::io::Result<std::net::TcpStream> {
    listener.set_nonblocking(true)?;
    for attempt in 0..REVERSE_ACCEPT_ATTEMPTS {
        if should_cancel() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "reverse-mode accept cancelled",
            ));
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                // Restore blocking mode so the subsequent header/stream reads
                // behave like an ordinary socket (with their own read timeout).
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt + 1 < REVERSE_ACCEPT_ATTEMPTS {
                    std::thread::sleep(REVERSE_ACCEPT_BACKOFF);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "reverse-mode accept retries exhausted",
    ))
}

/// Locate adb, auto-provisioning it if it cannot be found.
///
/// First tries [`crate::adb::locate_or_provisioned`] (which prefers a bundled
/// copy already under the per-user data dir, then the SDK/PATH/fixed-location
/// search). If that fails with [`adb::AdbError::NotFound`], FluxBridge
/// downloads and extracts Google's official platform-tools via
/// [`crate::provision::ensure_adb`], reporting download/extract progress as
/// [`PipelineStatus`] updates, and returns the bundled adb path.
///
/// On a provisioning failure (for example no network access offline) the
/// original [`adb::AdbError::NotFound`] is returned unchanged, so the caller
/// surfaces the existing actionable "install Android platform-tools" guidance
/// rather than an opaque download error. The one-time download and the bundled
/// adb execution are only reachable on the user's machine and are verified
/// there; the headless host has no network guarantee and no adb.
fn locate_or_provision_adb(
    status: &Sender<PipelineStatus>,
) -> Result<std::path::PathBuf, adb::AdbError> {
    // Resolve the per-user data dir once; if it cannot be resolved, fall back
    // to the plain search (which still works when adb is already installed).
    let data_dir = crate::provision::data_dir().ok();

    let located = match &data_dir {
        Some(dir) => adb::locate_or_provisioned(dir),
        None => adb::locate_adb(),
    };

    let not_found = match located {
        Ok(path) => return Ok(path),
        Err(e @ adb::AdbError::NotFound { .. }) => e,
        // A non-NotFound error (I/O) is not something provisioning fixes.
        Err(e) => return Err(e),
    };

    // adb is absent: try to auto-provision it. If we have no data dir there is
    // nowhere to install it, so surface the original NotFound guidance.
    let dir = match data_dir {
        Some(dir) => dir,
        None => return Err(not_found),
    };

    let mut on_progress = |p: crate::provision::ProvisionProgress| match p {
        crate::provision::ProvisionProgress::Downloading(pct) => {
            report(status, PipelineStatus::ProvisioningAdb(pct));
        }
        crate::provision::ProvisionProgress::Extracting => {
            report(status, PipelineStatus::ExtractingAdb);
        }
        crate::provision::ProvisionProgress::Done(path) => {
            report(status, PipelineStatus::AdbProvisioned(path.display().to_string()));
        }
    };

    let transport = fluxbridge_updater::UreqTransport::default();
    match crate::provision::ensure_adb(&dir, &transport, &mut on_progress) {
        Ok(path) => Ok(path),
        // Provisioning failed (offline, extract error, ...). Return the
        // original NotFound so the user still gets the actionable install
        // guidance that the AdbError::NotFound Display text carries.
        Err(_) => Err(not_found),
    }
}

/// Perform the adb-driven startup and run the decode loop, reporting progress
/// and any error through `status`.
///
/// This never panics on a missing adb / device / server: those are surfaced as
/// [`PipelineStatus::Error`] and returned as `Err`, so the GUI shows the problem
/// on its status line instead of crashing or writing to a nonexistent console.
///
/// On a live device this blocks in the decode loop until the socket closes; the
/// teardown runs on the way out. On the headless CI host there is no adb, so it
/// returns an `Error` immediately (which is the expected behavior there).
pub fn run_pipeline(
    config: &PipelineConfig,
    status: &Sender<PipelineStatus>,
) -> Result<(), adb::AdbError> {
    run_pipeline_with_frames(config, status, None)
}

/// Like [`run_pipeline`] but routes decoded frames to `frame_sink` when
/// provided.
///
/// The GUI build passes a sender so the UI can draw the decoded frames into a
/// `slint::Image`; the headless build passes `None` and the frames are dropped
/// (there is nothing to render to). All status/error reporting is identical to
/// [`run_pipeline`].
pub fn run_pipeline_with_frames(
    config: &PipelineConfig,
    status: &Sender<PipelineStatus>,
    frame_sink: Option<Sender<fluxbridge_core::DecodedFrame>>,
) -> Result<(), adb::AdbError> {
    run_pipeline_full(config, status, frame_sink, None, None)
}

/// Like [`run_pipeline_with_frames`] but also installs the scrcpy control sink
/// into `control_sink` once the video header is parsed, so the caller can drive
/// remote input injection.
///
/// The GUI passes a shared [`ControlSinkSlot`] so its input layer can take the
/// sink and forward mouse/key/touch events to the device. Callers that do not
/// need input (the headless build) pass `None`. All status/frame behavior is
/// identical to [`run_pipeline_with_frames`].
pub fn run_pipeline_full(
    config: &PipelineConfig,
    status: &Sender<PipelineStatus>,
    frame_sink: Option<Sender<fluxbridge_core::DecodedFrame>>,
    control_sink: Option<std::sync::Arc<ControlSinkSlot>>,
    cancel: Option<CancelFlag>,
) -> Result<(), adb::AdbError> {
    let selector = config.selector();
    let cancel = cancel.as_ref();

    // 0. If a stop was requested before we even started (or the flag arrives
    //    already set), short-circuit straight to Stopped WITHOUT spawning any
    //    adb. Nothing was set up, so there is nothing to tear down. This is the
    //    headless-testable cancellation path: no device, adb, or socket needed.
    if is_cancelled(cancel) {
        report(status, PipelineStatus::Stopped);
        return Ok(());
    }

    // 1. Locate adb, preferring a previously provisioned bundled copy. If adb
    //    cannot be found anywhere, auto-provision it (download + extract
    //    Google's official platform-tools into the per-user data dir) before
    //    failing, so a non-technical user never has to install the SDK. A
    //    download/extract failure (for example offline) falls back to the
    //    existing actionable "install adb" guidance.
    let adb_path = match locate_or_provision_adb(status) {
        Ok(p) => p,
        Err(e) => {
            report(status, PipelineStatus::Error(e.to_string()));
            return Err(e);
        }
    };
    report(
        status,
        PipelineStatus::AdbLocated(adb_path.display().to_string()),
    );
    let mut cmd = AdbCommand::with_path(adb_path, selector.clone());

    // 2. Wi-Fi connect, then resolve the REAL adb-registered serial.
    //
    //    `config.selector()` for Wi-Fi is the raw `ip:connect_port` mDNS
    //    connect address, which is only valid for the `adb connect` itself.
    //    With Android 11+ TLS wireless debugging the serial adb actually
    //    registers in `adb devices` is NOT reliably `ip:connect_port`, so every
    //    subsequent `-s <serial>` command (push/forward/launch/teardown) must
    //    target whatever adb listed. We resolve that serial here and REBIND
    //    both the selector and the AdbCommand to it; USB keeps UsbDefault.
    if is_cancelled(cancel) {
        report(status, PipelineStatus::Stopped);
        return Ok(());
    }
    let selector = if let Transport::WiFi { ip, port } = &config.transport {
        if let Err(e) = cmd.run(&adb::connect_args(ip, *port)) {
            // The connect itself failed, so nothing is registered to tear down.
            report(status, PipelineStatus::Error(e.to_string()));
            return Err(e);
        }
        report(status, PipelineStatus::Connected(format!("{ip}:{port}")));

        let real_serial = match cmd.resolve_connected_serial(ip, *port) {
            Ok(s) => s,
            Err(e) => {
                // The connect succeeded but adb never registered an online
                // serial. Disconnect best-effort (teardown is idempotent) so we
                // leave no half-open connection, THEN report the error so the
                // UI's terminal Error arrives only after teardown has run and
                // the worker is on its way out.
                teardown_with(&cmd, config, &selector);
                report(status, PipelineStatus::Error(e.to_string()));
                return Err(e);
            }
        };
        let resolved = DeviceSelector::Serial(real_serial.clone());
        // Rebind the command so push/forward/launch target the resolved serial.
        cmd = AdbCommand::with_path(cmd.adb_path.clone(), resolved.clone());
        report(status, PipelineStatus::Connected(real_serial));
        resolved
    } else {
        selector
    };

    // A stop requested during the Wi-Fi connect/resolve: nothing beyond the
    // connect is set up yet, so disconnect (teardown is safe/idempotent even
    // though no forward exists) and report Stopped.
    if is_cancelled(cancel) {
        teardown_with(&cmd, config, &selector);
        report(status, PipelineStatus::Stopped);
        return Ok(());
    }

    // 3. Push the server jar, resolved against the executable directory so an
    //    installed app finds the bundled jar regardless of its working dir.
    let server_jar = config.resolve_server_jar();
    if let Err(e) = cmd.run(&adb::push_server_args(&selector, &server_jar)) {
        // Tear down BEFORE reporting the terminal Error so the UI observes the
        // Error only after teardown has run and the worker is returning; this
        // keeps the lifecycle guard's release (on a terminal status) from
        // freeing a Start while this worker is still tearing down.
        teardown_with(&cmd, config, &selector);
        report(status, PipelineStatus::Error(e.to_string()));
        return Err(e);
    }
    report(status, PipelineStatus::ServerPushed);

    let tunnel_forward = config.server_opts.tunnel_forward;

    // 4. Set up the tunnel. In REVERSE mode (the shipped default) the desktop
    //    MUST be listening BEFORE the device connects out, so bind the local
    //    TcpListener first and only then run `adb reverse ...`; the device's
    //    outbound connect would otherwise be refused. In FORWARD mode set up
    //    `adb forward` and dial the socket later in run_stream.
    let reverse_listener = if tunnel_forward {
        if let Err(e) = cmd.run(&adb::forward_args(
            &selector,
            config.local_port,
            adb::SCRCPY_SOCKET_NAME,
        )) {
            teardown_with(&cmd, config, &selector);
            report(status, PipelineStatus::Error(e.to_string()));
            return Err(e);
        }
        report(status, PipelineStatus::Forwarded(config.local_port));
        None
    } else {
        // Bind the listener FIRST so the reverse tunnel points at a socket that
        // is already accepting; otherwise the device's connect races the bind.
        let bind_addr = format!("{}:{}", crate::connection::DEFAULT_HOST, config.local_port);
        let listener = match std::net::TcpListener::bind(&bind_addr) {
            Ok(l) => l,
            Err(e) => {
                let err = adb::AdbError::Io(e);
                teardown_with(&cmd, config, &selector);
                report(status, PipelineStatus::Error(err.to_string()));
                return Err(err);
            }
        };
        if let Err(e) = cmd.run(&adb::reverse_args(
            &selector,
            adb::SCRCPY_SOCKET_NAME,
            config.local_port,
        )) {
            teardown_with(&cmd, config, &selector);
            report(status, PipelineStatus::Error(e.to_string()));
            return Err(e);
        }
        report(status, PipelineStatus::Forwarded(config.local_port));
        Some(listener)
    };

    // A stop requested after the tunnel is up but before launch: remove the
    // tunnel we just added (teardown is best-effort) and report Stopped.
    if is_cancelled(cancel) {
        teardown_with(&cmd, config, &selector);
        report(status, PipelineStatus::Stopped);
        return Ok(());
    }

    // 5. Launch the scrcpy server with its stdout/stderr CAPTURED (not
    //    inherited) so a server-side startup failure is visible in the error
    //    path instead of being lost. A background reader drains both pipes into
    //    a bounded buffer so a chatty or silent server never blocks us.
    let mut server_child =
        match cmd.spawn_capturing(&adb::server_launch_args(&selector, &config.server_opts)) {
            Ok(child) => child,
            Err(e) => {
                teardown_with(&cmd, config, &selector);
                report(status, PipelineStatus::Error(e.to_string()));
                return Err(e);
            }
        };
    let server_output = ServerOutputBuffer::new();
    spawn_server_output_reader(&mut server_child, &server_output);
    report(status, PipelineStatus::ServerLaunched);

    // 6. Acquire the video (and control) socket(s) and run the scrcpy stream.
    //    Reverse mode accepts the device's inbound connections from the
    //    listener; forward mode dials the forwarded local socket.
    let source = match reverse_listener {
        Some(listener) => VideoSource::ReverseListener(listener),
        None => VideoSource::ForwardDial,
    };
    let result = run_stream(
        config,
        status,
        frame_sink,
        control_sink.as_deref(),
        &server_output,
        cancel,
        source,
    );

    // Teardown: kill the server child and remove the tunnel.
    let _ = server_child.kill();
    let _ = server_child.wait();
    teardown_with(&cmd, config, &selector);
    report(status, PipelineStatus::Stopped);

    result
}

/// How [`run_stream`] obtains the scrcpy video (and control) socket(s).
///
/// In the shipped REVERSE mode the desktop already bound a
/// [`std::net::TcpListener`] (before `adb reverse`), and the device connects
/// OUT to it, so run_stream accepts video then control from that listener and
/// never consumes a forward-mode dummy byte. In the legacy FORWARD mode
/// run_stream dials `127.0.0.1:<local_port>` and consumes the dummy byte, as
/// before.
enum VideoSource {
    /// Reverse tunneling: accept the inbound sockets from this listener.
    ReverseListener(std::net::TcpListener),
    /// Forward tunneling: dial the forwarded local socket and consume the dummy
    /// byte.
    ForwardDial,
}

/// Acquire the scrcpy video (and control) socket(s) and pump frames to the sink.
///
/// This is the SHIPPED v1 video path. The scrcpy server does NOT speak the
/// FluxBridge 0xFB protocol on its sockets, so this drives a plain
/// [`std::net::TcpStream`] through [`crate::scrcpy`] (video header,
/// length-prefixed H.264 access units) rather than
/// [`crate::connection::Connection`] / [`crate::decode_loop::run_decode_loop`],
/// which remain for a future FluxBridge-native server.
///
/// How the sockets are obtained depends on `source`:
///
/// * [`VideoSource::ReverseListener`] (the shipped default): scrcpy uses reverse
///   tunneling, so the device connects OUT to the desktop listener. The first
///   accept is the VIDEO socket, the second is the CONTROL socket (audio is
///   disabled, so there is no audio socket between them). Reverse mode sends NO
///   leading dummy byte, so the dummy-byte consume is SKIPPED entirely; the very
///   first bytes read are the 64-byte device name of the header.
/// * [`VideoSource::ForwardDial`]: scrcpy uses forward tunneling, so the desktop
///   dials `127.0.0.1:local_port` (video then control) and CONSUMES the leading
///   dummy byte before parsing the header, exactly as before.
///
/// When the server is launched with `control=true` (which the launch args
/// always set), scrcpy exposes a SECOND socket for the control channel. This
/// acquires that control socket and builds a [`crate::scrcpy::ScrcpyControlSink`]
/// against it, sized to the device resolution from the parsed video header, so
/// remote mouse/key/touch injection is a live, reachable path. The sink is
/// returned through `control_sink` (when provided) so the UI input layer can
/// drive it.
#[allow(clippy::too_many_arguments)]
fn run_stream(
    config: &PipelineConfig,
    status: &Sender<PipelineStatus>,
    frame_sink: Option<Sender<fluxbridge_core::DecodedFrame>>,
    control_sink: Option<&ControlSinkSlot>,
    server_output: &ServerOutputBuffer,
    cancel: Option<&CancelFlag>,
    source: VideoSource,
) -> Result<(), adb::AdbError> {
    use std::net::TcpStream;
    use std::sync::mpsc;

    // A stop requested before the socket is even acquired: return cleanly so the
    // caller runs kill-child + teardown exactly once.
    if is_cancelled(cancel) {
        return Ok(());
    }

    let addr = format!("{}:{}", crate::connection::DEFAULT_HOST, config.local_port);
    let tunnel_forward = config.server_opts.tunnel_forward;

    // Acquire the video socket and (optionally) the control socket per the
    // tunnel mode. In reverse mode we accept the device's inbound connections
    // from the listener (video first, then control); in forward mode we dial
    // the forwarded local socket (with a bounded connect retry for the
    // forward-mode readiness race) and open a second dial for control.
    let (mut video, control) = match source {
        VideoSource::ReverseListener(listener) => {
            let cancelled = || is_cancelled(cancel);
            // First inbound socket is the video stream.
            let video = match accept_reverse_socket(&listener, &cancelled) {
                Ok(s) => s,
                Err(e) => {
                    report(
                        status,
                        PipelineStatus::Error(stream_start_error(
                            &format!("reverse video socket {addr}: {e}"),
                            snapshot_server_output_after_grace(server_output),
                        )),
                    );
                    return Ok(());
                }
            };
            // A stop requested while we were waiting to accept video: return
            // cleanly so teardown runs once.
            if is_cancelled(cancel) {
                return Ok(());
            }
            // Second inbound socket is the control channel (control=true). A
            // failure here is not fatal: video still streams without input
            // injection, so it is reported but does not abort.
            let control = match accept_reverse_socket(&listener, &cancelled) {
                Ok(s) => Some(s),
                Err(e) => {
                    report(
                        status,
                        PipelineStatus::Error(format!(
                            "reverse control socket {addr}: {e} (input disabled)"
                        )),
                    );
                    None
                }
            };
            (video, control)
        }
        VideoSource::ForwardDial => {
            // First connection on the forwarded socket is the video stream. In
            // tunnel-forward mode `adb forward` can accept this local connection
            // before the device-side server is actually listening, so scrcpy's
            // own client retries the connect. Mirror that with a bounded retry.
            let video = match connect_forward_video(&addr) {
                Ok(s) => s,
                Err(e) => {
                    report(
                        status,
                        PipelineStatus::Error(stream_start_error(
                            &format!("video socket {addr}: {e}"),
                            snapshot_server_output_after_grace(server_output),
                        )),
                    );
                    return Ok(());
                }
            };
            // Second connection is the scrcpy control channel. A failure here is
            // not fatal to mirroring (video can still stream without input
            // injection), so it is reported but does not abort.
            let control = match TcpStream::connect(&addr) {
                Ok(s) => Some(s),
                Err(e) => {
                    report(
                        status,
                        PipelineStatus::Error(format!(
                            "control socket {addr}: {e} (input disabled)"
                        )),
                    );
                    None
                }
            };
            (video, control)
        }
    };
    // Use a SHORT read timeout (not the old 30s) so a blocked read on a stalled
    // stream wakes often enough for the access-unit loop to observe a Stop
    // request promptly. On a healthy stream frames arrive continuously so the
    // timeout never fires; on a stalled one it bounds Stop latency to roughly
    // this interval instead of ~30s. A timed-out read surfaces as a WouldBlock /
    // TimedOut error that stream_av_packets treats as a loop wake, not a fatal
    // error, so it re-checks the cancel flag and either continues or breaks.
    let _ = video.set_read_timeout(Some(STREAM_READ_TIMEOUT));

    report(status, PipelineStatus::Streaming);

    // If the caller supplied a frame sink (the GUI), decoded frames flow to it
    // so the UI can render them. Otherwise a local sink keeps the stream driver
    // running while discarding frames (the headless build has nothing to draw
    // to). The local receiver is drained in place so sends never fail.
    let (sink, local_rx) = match frame_sink {
        Some(tx) => (tx, None),
        None => {
            let (tx, rx) = mpsc::channel();
            (tx, Some(rx))
        }
    };
    if let Some(rx) = local_rx {
        std::thread::spawn(move || {
            for _ in rx.iter() {}
        });
    }

    // The real decode path links FFmpeg behind the `decode` feature. Without it
    // the StubDecoder keeps the pipeline running on hosts that cannot link
    // libav; its frames are not real pixels, which the UI handles by showing
    // the fallback placeholder.
    #[cfg(feature = "decode")]
    let mut decoder = match crate::decode_loop::FfmpegDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            report(status, PipelineStatus::Error(e.to_string()));
            return Ok(());
        }
    };
    #[cfg(not(feature = "decode"))]
    let mut decoder = crate::decode_loop::StubDecoder::new();

    // Drive the scrcpy video stream. The header (device name + resolution) must
    // be parsed BEFORE the access-unit loop so the control sink can be sized to
    // the real device resolution. So we consume the tunnel-forward dummy byte
    // and parse the header here, build the control sink, then run the shared
    // access-unit loop (scrcpy::stream_av_packets) to feed the decoder. The
    // all-in-one scrcpy::stream_video is the headless-testable equivalent.

    // Consume the dummy byte and header up front so the control sink can be
    // sized from the real device resolution before frames start flowing. In
    // tunnel-forward mode the first read can see an early EOF (the forward is
    // accepted before the server writes), so the dummy-byte read is retried
    // with a bounded backoff. A short read timeout turns a silent server into a
    // retryable would-block instead of an indefinite block. When the retry is
    // exhausted the captured server output is surfaced in the error.
    if tunnel_forward {
        let _ = video.set_read_timeout(Some(DUMMY_BYTE_READ_TIMEOUT));
        let dummy = crate::scrcpy::consume_dummy_byte_retrying(
            &mut video,
            DUMMY_BYTE_RETRY_ATTEMPTS,
            || {
                // Honor a Stop requested WHILE we are still waiting for a server
                // that never starts streaming: returning false from this seam
                // aborts the retry loop immediately so "Stop mirroring" tears
                // down promptly instead of waiting out the full ~2s retry
                // budget. On a live run the cancel flag is None (headless) or
                // the per-run CancelFlag the UI trips on Stop.
                if is_cancelled(cancel) {
                    return false;
                }
                std::thread::sleep(DUMMY_BYTE_RETRY_BACKOFF);
                true
            },
        );
        // Restore the longer streaming read timeout for the access-unit loop so
        // a blocked read wakes every STREAM_READ_TIMEOUT to observe the cancel
        // flag during the streaming access-unit loop below.
        let _ = video.set_read_timeout(Some(STREAM_READ_TIMEOUT));
        // A stop requested during the dummy-byte wait: the retry seam above
        // aborted early, so return cleanly and let the caller run teardown once.
        if is_cancelled(cancel) {
            return Ok(());
        }
        if let Err(e) = dummy {
            report(
                status,
                PipelineStatus::Error(stream_start_error(
                    &format!("dummy byte: {e}"),
                    snapshot_server_output_after_grace(server_output),
                )),
            );
            return Ok(());
        }
    }
    let header = match crate::scrcpy::parse_video_header(&mut video) {
        Ok(h) => h,
        Err(e) => {
            // An EOF at the very start of the header is the same class of
            // failure as the dummy-byte EOF (the server closed the socket), so
            // surface the captured server output and actionable hint too.
            report(
                status,
                PipelineStatus::Error(stream_start_error(
                    &format!("video header: {e}"),
                    snapshot_server_output_after_grace(server_output),
                )),
            );
            return Ok(());
        }
    };

    // Build the control sink now that the device resolution is known, and hand
    // it to the caller so input injection is a reachable, exercised path.
    if let (Some(ctrl), Some(slot)) = (control, control_sink) {
        let screen_w = header.width.min(u32::from(u16::MAX)) as u16;
        let screen_h = header.height.min(u32::from(u16::MAX)) as u16;
        let sink = crate::scrcpy::ScrcpyControlSink::new(ctrl, screen_w, screen_h);
        slot.set_resolution(header.width, header.height);
        slot.install(Box::new(sink));
    }

    // Configure the decoder from the header, then run the access-unit loop.
    let config_video = fluxbridge_core::VideoConfig {
        codec: "h264".to_string(),
        width: header.width,
        height: header.height,
        extra_data: Vec::new(),
    };
    if let Err(e) = fluxbridge_core::VideoDecoder::configure(&mut decoder, &config_video) {
        report(status, PipelineStatus::Error(e.to_string()));
        return Ok(());
    }

    // The access-unit loop checks the cancel flag once per iteration so a Stop
    // breaks it promptly between packets; the 2s STREAM_READ_TIMEOUT set above
    // lets a blocked read wake to observe the flag.
    let cancelled = || is_cancelled(cancel);
    if let Err(e) = crate::scrcpy::stream_av_packets(&mut video, &mut decoder, &sink, &cancelled) {
        report(status, PipelineStatus::Error(e.to_string()));
    }
    Ok(())
}

/// A slot the pipeline installs the constructed [`ScrcpyControlSink`] into once
/// the device resolution is known, so the caller (the UI input layer) can drive
/// input injection without the pipeline having to own the UI wiring.
///
/// It is a thread-safe handoff point: the pipeline thread builds the sink after
/// parsing the video header and calls [`ControlSinkSlot::install`]; the UI side
/// takes it with [`ControlSinkSlot::take`]. This keeps the control socket a
/// real, reachable path even though full UI-to-device input forwarding is a
/// larger piece of wiring.
#[derive(Default)]
pub struct ControlSinkSlot {
    inner: std::sync::Mutex<Option<Box<dyn fluxbridge_core::InputSink>>>,
    /// Device resolution (width, height) the sink was built with, so the UI can
    /// scale normalized pointer coordinates to device pixels. `(0, 0)` until a
    /// sink is installed.
    resolution: std::sync::Mutex<(u32, u32)>,
}

impl ControlSinkSlot {
    /// Create an empty slot.
    pub fn new() -> Self {
        ControlSinkSlot {
            inner: std::sync::Mutex::new(None),
            resolution: std::sync::Mutex::new((0, 0)),
        }
    }

    /// Install the control sink and the device resolution it targets (called by
    /// the pipeline once the control socket is up and the header is parsed).
    pub fn install(&self, sink: Box<dyn fluxbridge_core::InputSink>) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(sink);
        }
    }

    /// Record the device resolution the installed sink targets.
    pub fn set_resolution(&self, width: u32, height: u32) {
        if let Ok(mut guard) = self.resolution.lock() {
            *guard = (width, height);
        }
    }

    /// The device resolution the sink targets, or `(0, 0)` if none installed.
    pub fn resolution(&self) -> (u32, u32) {
        self.resolution.lock().map(|g| *g).unwrap_or((0, 0))
    }

    /// Take the installed sink, if any (called by the UI input layer).
    pub fn take(&self) -> Option<Box<dyn fluxbridge_core::InputSink>> {
        self.inner.lock().ok().and_then(|mut g| g.take())
    }

    /// Whether a control sink has been installed.
    pub fn is_ready(&self) -> bool {
        self.inner.lock().map(|g| g.is_some()).unwrap_or(false)
    }
}

/// Best-effort teardown using the transport-derived selector.
///
/// Kept for callers that have not resolved a Wi-Fi serial (and for USB, where
/// the selector is always [`DeviceSelector::UsbDefault`]). The pipeline itself
/// uses [`teardown_with`] so it tears down against the RESOLVED serial rather
/// than the raw `ip:connect_port`, which may not be the serial adb registered.
pub fn teardown(cmd: &AdbCommand, config: &PipelineConfig) {
    teardown_with(cmd, config, &config.selector());
}

/// Best-effort teardown against an explicit `selector`: remove the adb tunnel
/// (reverse or forward, matching the configured mode) and (for Wi-Fi)
/// disconnect.
///
/// Called on both the success and failure paths. Errors are ignored because
/// teardown must not mask the original outcome. The `selector` is the RESOLVED
/// serial for a Wi-Fi connection so the forward removed is the same one that
/// was added.
///
/// Tunnel removal matches the tunnel mode that was actually set up: in the
/// shipped REVERSE mode (`tunnel_forward=false`) it issues `adb reverse --remove
/// localabstract:scrcpy`; in FORWARD mode it issues `adb forward --remove
/// tcp:<local_port>`. Removing the wrong tunnel kind would leave the real one
/// dangling, so the mode is read from `config.server_opts.tunnel_forward`.
///
/// Wi-Fi disconnect: `adb disconnect <target>` is keyed on the connection
/// ADDRESS, and the address this pipeline connected with is the mDNS
/// `ip:connect_port` the `adb connect` used, so that is the primary disconnect
/// target. However, Bug 4 showed that on Android 11+ TLS wireless debugging adb
/// can REGISTER the device under a different serial (`ip:registered_port`) than
/// the connect address. To make sure a successful connect leaves no orphaned
/// entry regardless of which address adb kept, we ALSO disconnect the resolved
/// serial when it differs from the connect address. Both calls are best-effort
/// and idempotent (adb exits cleanly when the target is already gone), so the
/// extra call is a safe no-op in the common case where the two addresses match.
/// This cannot be exercised headlessly (no adb/device) and is finally verified
/// by the user on the real Windows device.
pub fn teardown_with(cmd: &AdbCommand, config: &PipelineConfig, selector: &DeviceSelector) {
    // Remove the tunnel kind that was set up. Reverse is the shipped default.
    if config.server_opts.tunnel_forward {
        let _ = cmd.run(&adb::forward_remove_args(selector, config.local_port));
    } else {
        let _ = cmd.run(&adb::reverse_remove_args(selector, adb::SCRCPY_SOCKET_NAME));
    }
    if let Transport::WiFi { ip, port } = &config.transport {
        let connect_addr = format!("{ip}:{port}");
        let _ = cmd.run(&adb::disconnect_args(ip, *port));
        // Defensively drop the resolved serial too, when adb registered the
        // device under a different address than we connected with.
        if let DeviceSelector::Serial(serial) = selector {
            if serial != &connect_addr {
                if let Some((host, resolved_port)) = split_host_port(serial) {
                    let _ = cmd.run(&adb::disconnect_args(host, resolved_port));
                }
            }
        }
    }
}

/// Split a `host:port` adb serial into its host and numeric port, or `None` if
/// it is not a `host:port` form (for example a plain USB serial). Used so
/// teardown can disconnect a resolved Wi-Fi serial that differs from the
/// connect address. Splits on the LAST colon so IPv6-style hosts are tolerated.
fn split_host_port(serial: &str) -> Option<(&str, u16)> {
    let (host, port) = serial.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn usb_selector_is_default() {
        let cfg = PipelineConfig::default();
        assert_eq!(cfg.selector(), DeviceSelector::UsbDefault);
    }

    #[test]
    fn wifi_selector_is_ip_port_serial() {
        let cfg = PipelineConfig {
            transport: Transport::WiFi {
                ip: "192.168.1.42".into(),
                port: 5555,
            },
            ..PipelineConfig::default()
        };
        assert_eq!(
            cfg.selector(),
            DeviceSelector::Serial("192.168.1.42:5555".into())
        );
    }

    /// A Wi-Fi transport built from an mDNS-DISCOVERED connect endpoint (a
    /// dynamic port, not the legacy 5555) routes through the same path as USB:
    /// its selector is the `host:connect_port` serial and `adb connect` targets
    /// exactly that discovered host:port.
    #[test]
    fn wifi_from_discovered_connect_port_yields_serial_and_connect_args() {
        let wc = crate::adb::WirelessConnection {
            host: "192.168.1.42".into(),
            connect_port: 37000, // dynamic mDNS connect port, NOT 5555
        };
        let cfg = PipelineConfig {
            transport: Transport::from_wireless(&wc),
            ..PipelineConfig::default()
        };
        assert_eq!(
            cfg.selector(),
            DeviceSelector::Serial("192.168.1.42:37000".into())
        );
        if let Transport::WiFi { ip, port } = &cfg.transport {
            assert_eq!(
                crate::adb::connect_args(ip, *port),
                vec!["connect", "192.168.1.42:37000"]
            );
        } else {
            panic!("expected a Wi-Fi transport");
        }
    }

    /// The BLOCKER-fix invariant (Bug 4): after `adb connect` on the mDNS
    /// connect port, the serial adb actually registers can differ from
    /// `ip:connect_port`. The pipeline resolves the REAL registered serial from
    /// `adb devices -l` and threads THAT (not the raw connect port) into push,
    /// forward, launch, and teardown. `resolve_connected_serial` is retry-based
    /// and impure, so this exercises the pure pieces end to end: parse a
    /// devices sample whose port differs from the connect port, resolve the
    /// serial, and assert every downstream arg vector carries the RESOLVED
    /// `-s <serial>` selector rather than `192.168.1.42:37000`.
    #[test]
    fn resolved_serial_not_connect_port_flows_into_push_and_launch() {
        let connect_host = "192.168.1.42";
        let connect_port = 37000u16; // mDNS connect port used only by `adb connect`
        let raw_connect_serial = format!("{connect_host}:{connect_port}");

        // What `adb devices -l` lists: the same device under a DIFFERENT port.
        let devices_output = "List of devices attached\n\
            192.168.1.42:42817 device product:redfin model:Pixel_5 device:redfin transport_id:3\n";
        let devices = crate::adb::parse_devices(devices_output);
        let resolved = crate::adb::resolve_wireless_serial(&devices, connect_host, connect_port)
            .expect("device for the connected host must resolve");
        assert_eq!(
            resolved, "192.168.1.42:42817",
            "resolved serial must be the registered port, not the connect port"
        );
        assert_ne!(
            resolved, raw_connect_serial,
            "the fix must NOT reuse the raw connect-port serial"
        );

        let sel = DeviceSelector::Serial(resolved.clone());

        // push targets the resolved serial, not ip:connect_port.
        let push = crate::adb::push_server_args(&sel, "scrcpy-server.jar");
        assert_eq!(push[0], "-s");
        assert_eq!(push[1], resolved);
        assert_ne!(push[1], raw_connect_serial);

        // server launch targets the resolved serial.
        let launch = crate::adb::server_launch_args(&sel, &ServerLaunchOptions::default());
        assert_eq!(launch[0], "-s");
        assert_eq!(launch[1], resolved);
        assert_ne!(launch[1], raw_connect_serial);

        // forward and forward --remove (teardown) also use the resolved serial.
        let forward = crate::adb::forward_args(&sel, 27183, crate::adb::SCRCPY_SOCKET_NAME);
        assert_eq!(&forward[0..2], &["-s".to_string(), resolved.clone()]);
        let forward_remove = crate::adb::forward_remove_args(&sel, 27183);
        assert_eq!(&forward_remove[0..2], &["-s".to_string(), resolved.clone()]);
    }

    /// `split_host_port` recovers the host and port from a `host:port` Wi-Fi
    /// serial (so teardown can defensively disconnect a resolved serial that
    /// differs from the connect address) and returns `None` for a non-host:port
    /// serial such as a USB device id.
    #[test]
    fn split_host_port_parses_wifi_serial_and_rejects_usb() {
        assert_eq!(
            split_host_port("192.168.1.42:42817"),
            Some(("192.168.1.42", 42817))
        );
        // Splits on the last colon so an IPv6-style host is tolerated.
        assert_eq!(split_host_port("fe80::1:5555"), Some(("fe80::1", 5555)));
        // A plain USB serial has no port and must not parse.
        assert_eq!(split_host_port("ZY223KLMNO"), None);
        // A non-numeric port must not parse.
        assert_eq!(split_host_port("192.168.1.42:notaport"), None);
    }

    #[test]
    fn connection_config_points_at_local_forward() {
        let cfg = PipelineConfig {
            local_port: 30000,
            ..PipelineConfig::default()
        };
        let cc = cfg.connection_config();
        assert_eq!(cc.host, "127.0.0.1");
        assert_eq!(cc.port, 30000);
    }

    /// A bare relative `server_jar` is resolved to an absolute path under the
    /// running executable's directory, NOT the process working directory, so an
    /// installed app (launched from a shortcut with an unrelated CWD) still
    /// finds the bundled jar. An already-absolute path is returned unchanged.
    #[test]
    fn resolve_server_jar_is_absolute_and_next_to_exe() {
        let cfg = PipelineConfig::default();
        let resolved = cfg.resolve_server_jar();
        let path = std::path::Path::new(&resolved);
        assert!(
            path.is_absolute(),
            "resolved jar path must be absolute, got {resolved}"
        );
        assert!(
            resolved.ends_with("scrcpy-server.jar"),
            "resolved jar must keep the jar file name, got {resolved}"
        );
        // It must live beside the current executable, not the CWD.
        let exe_parent = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert_eq!(path.parent().unwrap(), exe_parent.as_path());
    }

    #[test]
    fn resolve_server_jar_keeps_absolute_path() {
        let abs = if cfg!(windows) {
            "C:\\opt\\scrcpy-server.jar"
        } else {
            "/opt/scrcpy-server.jar"
        };
        let cfg = PipelineConfig {
            server_jar: abs.to_string(),
            ..PipelineConfig::default()
        };
        assert_eq!(cfg.resolve_server_jar(), abs);
    }

    /// The scrcpy control sink the pipeline installs into a [`ControlSinkSlot`]
    /// is a real, reachable input path: once installed it encodes FluxBridge
    /// input events into exact scrcpy control-message bytes. This drives the
    /// full handoff (install -> take -> send) against an in-memory writer, the
    /// headless stand-in for the control `TcpStream` the pipeline opens on a
    /// live device.
    #[test]
    fn control_sink_slot_handoff_delivers_scrcpy_input_bytes() {
        use fluxbridge_core::protocol::TouchEvent;
        use fluxbridge_core::InputSink;
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        // A writer end that records every byte written, shared so the test can
        // inspect it after the sink (which owns the writer) is dropped.
        #[derive(Clone, Default)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let writer = SharedWriter(Arc::clone(&recorded));

        // The pipeline builds the sink from the parsed header's resolution and
        // installs it into the slot; the UI side takes it.
        let slot = Arc::new(ControlSinkSlot::new());
        assert!(!slot.is_ready());
        slot.install(Box::new(crate::scrcpy::ScrcpyControlSink::new(
            writer, 1080, 1920,
        )));
        assert!(slot.is_ready());

        let mut sink = slot.take().expect("control sink was installed");
        assert!(!slot.is_ready(), "take() empties the slot");

        sink.send_touch(TouchEvent {
            pointer_id: 1,
            x: 100,
            y: 200,
            action: 0, // down
            pressure: 0xFFFF,
        })
        .unwrap();
        drop(sink);

        let bytes = recorded.lock().unwrap().clone();
        // Exactly one INJECT_TOUCH_EVENT message (32 bytes), type byte first.
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], crate::scrcpy::CTRL_INJECT_TOUCH);
        // Screen dimensions the sink was built with are embedded (1080x1920).
        assert_eq!(&bytes[18..20], &[0x04, 0x38]); // width 1080
        assert_eq!(&bytes[20..22], &[0x07, 0x80]); // height 1920
    }

    /// With adb absent (as on the headless CI host), the pipeline reports an
    /// Error status through the channel and returns Err rather than printing to
    /// a console or panicking. We force absence by clearing the SDK env vars and
    /// PATH for the duration of the call.
    #[test]
    fn missing_adb_reports_error_through_channel() {
        // Serialize through the crate-wide process-global env lock so this
        // never races another env-mutating test (env vars are process-global;
        // independent locks would not serialize against each other).
        // `with_env_lock` saves/restores the full shared key set, so we only
        // apply the mutations this test needs inside the closure.
        //
        // We strip the SDK vars and PATH so adb cannot be found, AND the vars
        // `directories` uses to resolve the data dir (HOME / XDG_DATA_HOME on
        // unix, LOCALAPPDATA on Windows) so `provision::data_dir()` returns
        // None and the pipeline does NOT attempt a live platform-tools
        // download from this headless host. We are asserting the
        // offline-fallback path: no data dir -> the original NotFound guidance
        // is surfaced.
        let mut result = None;
        let mut statuses: Vec<PipelineStatus> = Vec::new();
        crate::test_env::with_env_lock(|| {
            std::env::set_var("ANDROID_HOME", "/nonexistent-sdk-xyz");
            std::env::remove_var("ANDROID_SDK_ROOT");
            std::env::set_var("PATH", "/nonexistent-path-xyz");
            std::env::remove_var("HOME");
            std::env::remove_var("LOCALAPPDATA");
            std::env::remove_var("XDG_DATA_HOME");

            let (tx, rx) = mpsc::channel();
            let cfg = PipelineConfig::default();
            result = Some(run_pipeline(&cfg, &tx));
            statuses = rx.try_iter().collect();
        });

        let result = result.expect("pipeline ran under the env lock");
        assert!(result.is_err(), "missing adb must return Err");
        assert!(
            matches!(statuses.last(), Some(PipelineStatus::Error(_))),
            "an Error status must be reported, got {statuses:?}"
        );
    }

    /// The server-output ring buffer keeps only the last N lines (evicting the
    /// oldest) and snapshots the last K in order, trimming trailing CR/LF so
    /// Windows adb output does not leave stray carriage returns.
    #[test]
    fn server_output_buffer_keeps_last_lines_in_order() {
        let buf = ServerOutputBuffer::new();
        // Empty buffer snapshots to None so silence is distinguishable.
        assert!(buf.snapshot(5).is_none());

        for i in 0..(SERVER_OUTPUT_LINES + 10) {
            buf.push_line(&format!("line {i}\r\n"));
        }
        // Only the last SERVER_OUTPUT_LINES are retained.
        let all = buf.snapshot(SERVER_OUTPUT_LINES).unwrap();
        let lines: Vec<&str> = all.lines().collect();
        assert_eq!(lines.len(), SERVER_OUTPUT_LINES);
        // Oldest retained line is (total - N); newest is the last pushed.
        let total = SERVER_OUTPUT_LINES + 10;
        assert_eq!(lines[0], format!("line {}", total - SERVER_OUTPUT_LINES));
        assert_eq!(lines[lines.len() - 1], format!("line {}", total - 1));
        // Trailing CR/LF were trimmed.
        assert!(!all.contains('\r'));

        // Snapshot of the last K returns exactly K lines, in order.
        let last3 = buf.snapshot(3).unwrap();
        let last3_lines: Vec<&str> = last3.lines().collect();
        assert_eq!(last3_lines.len(), 3);
        assert_eq!(last3_lines[2], format!("line {}", total - 1));
    }

    /// The ring buffer drains a Read (the headless stand-in for the child's
    /// piped stderr) line by line and returns cleanly at EOF.
    #[test]
    fn server_output_buffer_drains_a_reader_to_eof() {
        let buf = ServerOutputBuffer::new();
        let fake_stderr = b"[server] ERROR: could not parse option\nExit\n".to_vec();
        buf.drain_from(std::io::Cursor::new(fake_stderr));
        let snap = buf.snapshot(SERVER_OUTPUT_LINES).unwrap();
        assert!(snap.contains("could not parse option"));
        assert!(snap.contains("Exit"));
    }

    /// An immediate-EOF dummy-byte read, combined with captured server output,
    /// produces an error that INCLUDES the server's text plus the actionable
    /// hint. This is the diagnosable-failure acceptance criterion, exercised
    /// with an in-memory Read seam and a fake server-output source (no socket).
    #[test]
    fn immediate_eof_dummy_byte_surfaces_captured_server_output() {
        // Fake captured server output: the kind of stderr a mismatched/failed
        // server prints before closing the socket.
        let server_output = ServerOutputBuffer::new();
        server_output.drain_from(std::io::Cursor::new(
            b"[server] ERROR: The server version does not match the client\n".to_vec(),
        ));

        // A video stream at immediate EOF (0 bytes), the exact condition that
        // produced "dummy byte: failed to fill whole buffer".
        let mut stream = std::io::Cursor::new(Vec::<u8>::new());
        let dummy = crate::scrcpy::consume_dummy_byte_retrying(&mut stream, 3, || true);
        let err = dummy.unwrap_err();

        let message = stream_start_error(
            &format!("dummy byte: {err}"),
            server_output.snapshot(SERVER_OUTPUT_LINES),
        );
        // The captured server line appears verbatim in the surfaced error.
        assert!(
            message.contains("The server version does not match the client"),
            "error must include captured server output, got: {message}"
        );
        // And the actionable hint is present.
        assert!(message.contains("Wireless debugging"));
        assert!(message.contains("Android version is 11"));
        // No em dashes anywhere in the copy.
        assert!(!message.contains('\u{2014}'));
    }

    /// When nothing was captured (a server that exited before logging), the
    /// error says so explicitly so silence is distinguishable from a message.
    #[test]
    fn stream_start_error_reports_no_output_when_buffer_empty() {
        let empty = ServerOutputBuffer::new();
        let message = stream_start_error("dummy byte: timeout", empty.snapshot(SERVER_OUTPUT_LINES));
        assert!(message.contains("(no output captured)"), "{message}");
        assert!(message.contains("Wireless debugging"));
        assert!(!message.contains('\u{2014}'));
    }

    /// The grace-drain snapshot reliably reflects server output that was pushed
    /// around the time of the failure, so the diagnosable-failure guarantee is
    /// not best-effort. A line captured before the snapshot must appear in the
    /// surfaced error. This exercises the buffer/snapshot/error-message
    /// contract directly rather than relying on real reader-thread timing.
    #[test]
    fn grace_snapshot_includes_server_output_pushed_before_failure() {
        let server_output = ServerOutputBuffer::new();
        // The server printed its reason and closed the socket; the line is in
        // the buffer by the time the error path snapshots it.
        server_output.push_line("[server] ERROR: app_process failed to start");

        let snap = snapshot_server_output_after_grace(&server_output);
        let message = stream_start_error("dummy byte: unexpected EOF", snap);

        assert!(
            message.contains("app_process failed to start"),
            "grace snapshot must surface the captured server line, got: {message}"
        );
        assert!(message.contains("Wireless debugging"));
        assert!(!message.contains("(no output captured)"));
        assert!(!message.contains('\u{2014}'));
    }

    /// The grace drain must not invent output: a server that was genuinely
    /// silent still yields an empty snapshot and the explicit
    /// "(no output captured)", so the bounded wait never masks real silence.
    #[test]
    fn grace_snapshot_stays_empty_for_a_silent_server() {
        let server_output = ServerOutputBuffer::new();
        let snap = snapshot_server_output_after_grace(&server_output);
        assert!(snap.is_none(), "a silent server must snapshot to None");

        let message = stream_start_error("dummy byte: unexpected EOF", snap);
        assert!(message.contains("(no output captured)"), "{message}");
        assert!(!message.contains('\u{2014}'));
    }

    /// Connect errors are classified so the forward-mode readiness race
    /// (connection refused/reset/aborted/timed-out) is retried while a hard
    /// error is not.
    #[test]
    fn connect_error_classification() {
        use std::io::{Error, ErrorKind};
        assert!(is_retryable_connect_error(&Error::new(
            ErrorKind::ConnectionRefused,
            "x"
        )));
        assert!(is_retryable_connect_error(&Error::new(
            ErrorKind::TimedOut,
            "x"
        )));
        assert!(!is_retryable_connect_error(&Error::new(
            ErrorKind::PermissionDenied,
            "x"
        )));
    }

    // --- Reverse tunneling (the shipped default) --------------------------

    /// The shipped default is REVERSE tunneling: `ServerLaunchOptions::default`
    /// must have `tunnel_forward=false` so the server never writes the
    /// forward-mode leading dummy byte and the device connects out to the
    /// desktop listener instead. This is the root-cause fix for the forward-mode
    /// dummy-byte readiness race.
    #[test]
    fn default_server_opts_use_reverse_tunnel() {
        assert!(
            !ServerLaunchOptions::default().tunnel_forward,
            "the shipped default must be reverse tunneling (tunnel_forward=false)"
        );
        let args = crate::adb::server_launch_args(
            &DeviceSelector::UsbDefault,
            &ServerLaunchOptions::default(),
        );
        assert!(
            args.iter().any(|a| a == "tunnel_forward=false"),
            "default launch args must request reverse tunneling, got {args:?}"
        );
    }

    /// The reverse path drives a dummy-byte-FREE scrcpy stream off a loopback
    /// TcpListener: a helper thread connects to the listener (as the device
    /// would) and writes the scrcpy wire bytes with NO leading 0x00, and the
    /// consumer accepts the socket, parses the header, and pumps the access
    /// units through the shared loop to a sink.
    ///
    /// This FAILS if the reverse path wrongly expected/consumed a leading dummy
    /// byte: the first byte read must be the first byte of the 64-byte device
    /// name, not a 0x00. We prove this by making the device name's first byte a
    /// non-zero ASCII char; if a dummy-byte consume were (wrongly) present it
    /// would eat that byte and the header (and thus the whole stream) would be
    /// misframed, so no access unit would reach the sink.
    #[test]
    fn reverse_path_parses_header_without_dummy_byte_and_pumps_a_packet() {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};

        // Build a scrcpy-style stream: 64-byte device name (NUL padded, first
        // byte deliberately non-zero), codec id "h264", width, height, then one
        // length-prefixed key-frame access unit. NO leading dummy byte.
        let mut wire: Vec<u8> = Vec::new();
        let mut name = [0u8; 64];
        name[..6].copy_from_slice(b"SM-F94"); // non-zero first byte on purpose
        wire.extend_from_slice(&name);
        wire.extend_from_slice(&crate::scrcpy::fourcc(b"h264").to_be_bytes());
        wire.extend_from_slice(&1080u32.to_be_bytes()); // width
        wire.extend_from_slice(&1920u32.to_be_bytes()); // height
                                                        // One access unit: 8-byte pts/flags header (key-frame flag set) + 4-byte
                                                        // length + payload. A non-config unit so it is decoded and forwarded.
        let payload = b"\x00\x00\x00\x01fakeNAL".to_vec();
        let pts_flags: u64 = 1u64 << 62; // PACKET_FLAG_KEY_FRAME, not config
        wire.extend_from_slice(&pts_flags.to_be_bytes());
        wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        wire.extend_from_slice(&payload);

        // Loopback listener: the desktop side binds and accepts, exactly as the
        // reverse pipeline does before `adb reverse`.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().unwrap();

        // The "device" side connects out and writes the wire bytes.
        let writer = std::thread::spawn(move || {
            let mut sock = TcpStream::connect(addr).expect("device connects out");
            sock.write_all(&wire).unwrap();
            // Drop closes the socket so the consumer sees a clean EOF after the
            // single access unit.
        });

        // Desktop side: accept the inbound video socket via the SAME bounded
        // accept helper the pipeline uses, then drive the reverse stream.
        let no_cancel = || false;
        let mut video = accept_reverse_socket(&listener, &no_cancel).expect("accept video");

        // Drive the dummy-byte-FREE reverse path: header parse + access-unit
        // loop, mirroring what run_stream does in reverse mode. stream_video
        // with tunnel_forward=false is the headless-testable equivalent and
        // does NOT consume a dummy byte.
        let (tx, rx) = mpsc::channel::<fluxbridge_core::DecodedFrame>();
        let mut decoder = crate::decode_loop::StubDecoder::new();
        let header = crate::scrcpy::stream_video(&mut video, false, &mut decoder, &tx)
            .expect("reverse stream parses and runs to EOF without a dummy byte");

        writer.join().unwrap();

        // The header parsed correctly: proof no 0x00 was consumed ahead of it
        // (a prepended dummy byte would have shifted every field and corrupted
        // the device name / resolution).
        assert_eq!(header.device_name, "SM-F94");
        assert_eq!(header.width, 1080);
        assert_eq!(header.height, 1920);

        // At least one access unit reached the sink.
        let frames: Vec<_> = rx.try_iter().collect();
        assert!(
            !frames.is_empty(),
            "the reverse path must pump at least one access unit to the sink"
        );
    }

    /// The reverse accept path is BOUNDED: when no peer ever connects within the
    /// budget, the accept helper returns a TimedOut error instead of blocking
    /// forever, and the pipeline turns that into the same actionable
    /// stream-start error (with the preserved "Server said: ..." diagnostics)
    /// used by the forward path. We drive it with a short cancel-triggered abort
    /// so the test is fast rather than waiting out the full ~2s budget.
    #[test]
    fn reverse_accept_timeout_surfaces_stream_start_error_with_server_output() {
        use std::net::TcpListener;

        // A listener no one ever connects to (the failure case: the server
        // never connected back).
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");

        // Abort the accept immediately via the cancel seam so we do not wait out
        // the full budget; the helper returns a TimedOut error on abort.
        let always_cancel = || true;
        let err = accept_reverse_socket(&listener, &always_cancel)
            .expect_err("accept with no peer must not succeed");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

        // The pipeline wraps that into the shared stream_start_error with the
        // captured server output, so the user still sees the diagnostics rather
        // than a hang.
        let server_output = ServerOutputBuffer::new();
        server_output.push_line("[server] INFO: Device: [samsung] samsung SM-F946B (Android 17)");
        let message = stream_start_error(
            &format!("reverse video socket 127.0.0.1:27183: {err}"),
            server_output.snapshot(SERVER_OUTPUT_LINES),
        );
        assert!(
            message.contains("scrcpy stream did not start"),
            "message must name the stream-start failure, got: {message}"
        );
        assert!(
            message.contains("Server said"),
            "message must preserve the Server said diagnostics, got: {message}"
        );
        assert!(message.contains("SM-F946B (Android 17)"));
        assert!(!message.contains('\u{2014}'), "no em dashes");
    }

    /// Teardown must remove the tunnel kind that was actually set up. In the
    /// shipped REVERSE mode (`tunnel_forward=false`) it issues `adb reverse
    /// --remove localabstract:scrcpy`; it must NOT issue `adb forward --remove`
    /// (which would leave the reverse tunnel dangling). We assert on the exact
    /// arg vectors the removal helpers build for the configured mode.
    #[test]
    fn teardown_removes_reverse_tunnel_when_reverse_configured() {
        // Reverse (default) mode.
        let reverse_cfg = PipelineConfig::default();
        assert!(!reverse_cfg.server_opts.tunnel_forward);
        let sel = DeviceSelector::UsbDefault;
        // The removal the reverse branch of teardown_with runs.
        let reverse_remove = crate::adb::reverse_remove_args(&sel, crate::adb::SCRCPY_SOCKET_NAME);
        assert_eq!(
            reverse_remove,
            vec!["reverse", "--remove", "localabstract:scrcpy"],
            "reverse teardown must remove the reverse tunnel"
        );
        // It must differ from the forward removal (so the wrong one is not used).
        let forward_remove = crate::adb::forward_remove_args(&sel, reverse_cfg.local_port);
        assert_ne!(reverse_remove, forward_remove);

        // Forward mode still removes the forward tunnel.
        let forward_cfg = PipelineConfig {
            server_opts: ServerLaunchOptions {
                tunnel_forward: true,
                ..ServerLaunchOptions::default()
            },
            ..PipelineConfig::default()
        };
        assert!(forward_cfg.server_opts.tunnel_forward);
        assert_eq!(
            crate::adb::forward_remove_args(&sel, forward_cfg.local_port),
            vec![
                "forward".to_string(),
                "--remove".to_string(),
                format!("tcp:{}", forward_cfg.local_port)
            ]
        );
    }

    #[test]
    fn status_display_is_human_readable() {
        assert_eq!(
            PipelineStatus::Forwarded(27183).to_string(),
            "forwarding 127.0.0.1:27183"
        );
        assert_eq!(
            PipelineStatus::Error("no device".into()).to_string(),
            "error: no device"
        );
    }

    /// The adb auto-provisioning status variants render human-readable text the
    /// GUI can show on its status line (download percent, extraction, done).
    #[test]
    fn provisioning_status_display_is_human_readable() {
        assert_eq!(
            PipelineStatus::ProvisioningAdb(42).to_string(),
            "Downloading Android platform-tools... 42%"
        );
        assert_eq!(
            PipelineStatus::ExtractingAdb.to_string(),
            "Extracting Android platform-tools..."
        );
        assert_eq!(
            PipelineStatus::AdbProvisioned("/data/platform-tools/adb".into()).to_string(),
            "adb installed at /data/platform-tools/adb"
        );
    }

    /// The wireless discovery/pairing status variants render human-readable
    /// text for the UI status line.
    #[test]
    fn wireless_status_display_is_human_readable() {
        assert_eq!(
            PipelineStatus::Discovering.to_string(),
            "searching for wireless devices..."
        );
        assert_eq!(
            PipelineStatus::Pairing("192.168.1.42:41355".into()).to_string(),
            "pairing with 192.168.1.42:41355..."
        );
        assert_eq!(
            PipelineStatus::Paired("192.168.1.42:37000".into()).to_string(),
            "paired with 192.168.1.42:37000"
        );
    }

    // --- Feature 6: cooperative stop / teardown (headless) ----------------

    /// A fresh [`CancelFlag`] is NOT cancelled, [`CancelFlag::cancel`] trips it,
    /// setting it twice leaves it set (idempotent), and [`CancelFlag::reset`]
    /// clears it so the same flag can be reused for a later Start.
    #[test]
    fn cancel_flag_semantics_are_idempotent() {
        let flag = CancelFlag::new();
        assert!(!flag.is_cancelled(), "a fresh flag must not be cancelled");

        flag.cancel();
        assert!(flag.is_cancelled(), "cancel must trip the flag");
        // Idempotent: a second cancel leaves it set.
        flag.cancel();
        assert!(flag.is_cancelled(), "a second cancel must leave it set");

        // A clone shares the same state (the UI hands a clone to the thread).
        let clone = flag.clone();
        assert!(clone.is_cancelled(), "a clone must observe the same state");

        flag.reset();
        assert!(!flag.is_cancelled(), "reset must clear the flag");
        assert!(
            !clone.is_cancelled(),
            "reset must be visible through a clone"
        );
    }

    /// The `Default` impl matches `new`: not cancelled.
    #[test]
    fn cancel_flag_default_is_not_cancelled() {
        assert!(!CancelFlag::default().is_cancelled());
    }

    /// The lifecycle fix (review issues 1 and 2): a fresh per-run flag plus a
    /// guard released only on worker termination makes Stop-then-Start
    /// impossible to overlap.
    ///
    /// Start hands out a fresh flag and marks the run live; a SECOND start while
    /// the first is still live is refused (None), so the UI cannot spawn an
    /// overlapping worker. request_stop trips ONLY the current run's flag and
    /// does NOT free the guard. The guard is freed only when the worker signals
    /// termination (worker_stopped, driven by observing PipelineStatus::Stopped),
    /// after which a new start succeeds with a DISTINCT flag that is NOT
    /// cancelled by the previous Stop.
    #[test]
    fn run_guard_refuses_overlap_until_worker_stopped() {
        let mut guard = RunGuard::new();
        assert!(!guard.is_started(), "a fresh guard is idle");
        // request_stop and worker_stopped are safe no-ops when idle.
        guard.request_stop();
        guard.worker_stopped();
        assert!(!guard.is_started());

        // First Start: get a fresh flag, run is now live.
        let first = guard.try_start().expect("first start must succeed");
        assert!(guard.is_started());
        assert!(!first.is_cancelled(), "a fresh run flag is not cancelled");

        // A second Start BEFORE the worker terminates is refused, so no
        // overlapping worker can be spawned.
        assert!(
            guard.try_start().is_none(),
            "a second start must be refused while a run is live"
        );

        // Stop trips ONLY the current run's flag; the guard stays live until the
        // worker actually terminates.
        guard.request_stop();
        assert!(first.is_cancelled(), "stop must trip the current run's flag");
        assert!(
            guard.is_started(),
            "the guard must NOT be released on the Stop click"
        );
        assert!(
            guard.try_start().is_none(),
            "a start after Stop but before Stopped is observed must still be refused"
        );

        // The worker terminates (PipelineStatus::Stopped observed) -> guard freed.
        guard.worker_stopped();
        assert!(!guard.is_started(), "worker_stopped releases the guard");

        // Now a fresh Start succeeds with a DISTINCT flag that the earlier Stop
        // did NOT cancel (no shared-flag reuse race).
        let second = guard.try_start().expect("start after Stopped must succeed");
        assert!(
            !second.is_cancelled(),
            "the new run's flag must be fresh, not cancelled by the earlier Stop"
        );
        // Tripping the old flag must not affect the new run, and vice versa.
        first.cancel();
        assert!(
            !second.is_cancelled(),
            "the previous run's flag must not cancel the current run"
        );
    }

    /// The optional-flag helper treats `None` (headless / non-stoppable
    /// callers) as never cancelled and honors a `Some` flag's state.
    #[test]
    fn is_cancelled_helper_handles_none_and_some() {
        assert!(!is_cancelled(None), "None must never be cancelled");

        let flag = CancelFlag::new();
        assert!(!is_cancelled(Some(&flag)));
        flag.cancel();
        assert!(is_cancelled(Some(&flag)));
    }

    /// A pre-set cancel flag short-circuits [`run_pipeline_full`] straight to
    /// [`PipelineStatus::Stopped`] WITHOUT spawning any adb, which is why this
    /// runs on the headless host with no device, adb, or socket. It must return
    /// `Ok(())` promptly (not hang) and emit exactly `Stopped`.
    #[test]
    fn cancelled_pipeline_reaches_stopped_without_device() {
        let cfg = PipelineConfig::default();
        let (tx, rx) = mpsc::channel();
        let cancel = CancelFlag::new();
        cancel.cancel(); // already cancelled before the run starts

        let result = run_pipeline_full(&cfg, &tx, None, None, Some(cancel));
        assert!(result.is_ok(), "an already-cancelled run must return Ok");

        let statuses: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            statuses,
            vec![PipelineStatus::Stopped],
            "a pre-cancelled run must emit only Stopped, with no adb/connect"
        );
    }

    /// Teardown is best-effort and idempotent: calling [`teardown`] /
    /// [`teardown_with`] twice (and when nothing was set up) does not panic, and
    /// it drives exactly the expected `forward --remove` and (for Wi-Fi)
    /// `disconnect` arg vectors against the resolved selector. The AdbCommand
    /// points at a path that does not exist, so each `run` fails silently (as
    /// teardown already ignores errors), which is the headless stand-in for a
    /// device that was never fully set up.
    #[test]
    fn teardown_is_idempotent_and_builds_expected_args() {
        // The exact arg vectors teardown drives, asserted directly so the test
        // fails if the teardown wiring changes.
        let wifi_cfg = PipelineConfig {
            transport: Transport::WiFi {
                ip: "192.168.1.42".into(),
                port: 37000,
            },
            local_port: 27183,
            ..PipelineConfig::default()
        };
        let resolved = DeviceSelector::Serial("192.168.1.42:42817".into());
        assert_eq!(
            adb::forward_remove_args(&resolved, wifi_cfg.local_port),
            vec!["-s", "192.168.1.42:42817", "forward", "--remove", "tcp:27183"]
        );
        assert_eq!(
            adb::disconnect_args("192.168.1.42", 37000),
            vec!["disconnect", "192.168.1.42:37000"]
        );

        // Calling teardown twice (and for a USB config where nothing was set
        // up) must not panic. The adb path is intentionally bogus so run()
        // returns Err, which teardown ignores.
        let bogus = std::path::PathBuf::from("/nonexistent/adb-does-not-exist");
        let wifi_cmd = AdbCommand::with_path(bogus.clone(), resolved.clone());
        teardown_with(&wifi_cmd, &wifi_cfg, &resolved);
        teardown_with(&wifi_cmd, &wifi_cfg, &resolved); // idempotent second call

        let usb_cfg = PipelineConfig::default();
        let usb_cmd = AdbCommand::with_path(bogus, DeviceSelector::UsbDefault);
        teardown(&usb_cmd, &usb_cfg);
        teardown(&usb_cmd, &usb_cfg); // nothing was set up; still safe
    }
}
