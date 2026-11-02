//! adb-driven mirroring pipeline: startup, decode, and teardown.
//!
//! This module owns the real connection lifecycle that `main.rs` kicks off:
//!
//! 1. Locate `adb` on the host ([`crate::adb::locate_adb`]).
//! 2. For Wi-Fi, `adb connect <ip:port>`; for USB, use the sole attached device.
//! 3. Push the scrcpy server jar and set up the adb `forward` tunnel.
//! 4. Launch the scrcpy server via `app_process`.
//! 5. Connect the local TCP socket adb forwards and run the decode loop.
//! 6. On exit (or any failure) tear down: remove the forward, kill the server
//!    child, and disconnect Wi-Fi.
//!
//! Every notable transition and every failure is reported as a
//! [`PipelineStatus`] on an `mpsc` channel. The GUI build drains that channel
//! onto a visible status line instead of relying on a console that may not
//! exist (a windowed Windows app has no attached stdout). The headless build
//! may print the same statuses to stdout.
//!
//! The adb teardown commands and the status-reporting flow are unit-tested;
//! the parts that require a live device (`push`, `app_process`, the socket) are
//! only reached at runtime on the user's machine, never on the headless CI host.

use std::fmt;
use std::sync::mpsc::Sender;

use crate::adb::{self, AdbCommand, DeviceSelector, ServerLaunchOptions};
use crate::connection::ConnectionConfig;

/// How the device is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// USB: a single attached device, tunneled with `adb forward`.
    Usb,
    /// Wi-Fi: `adb connect <ip:port>` first, then the same forward tunnel.
    WiFi {
        /// Device IP address.
        ip: String,
        /// Device adb TCP/IP port (typically 5555).
        port: u16,
    },
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
fn locate_or_provision_adb(status: &Sender<PipelineStatus>) -> Result<std::path::PathBuf, adb::AdbError> {
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
    run_pipeline_full(config, status, frame_sink, None)
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
) -> Result<(), adb::AdbError> {
    let selector = config.selector();

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
    let cmd = AdbCommand::with_path(adb_path, selector.clone());

    // 2. Wi-Fi connect.
    if let Transport::WiFi { ip, port } = &config.transport {
        if let Err(e) = cmd.run(&adb::connect_args(ip, *port)) {
            report(status, PipelineStatus::Error(e.to_string()));
            return Err(e);
        }
        report(status, PipelineStatus::Connected(format!("{ip}:{port}")));
    }

    // 3. Push the server jar, resolved against the executable directory so an
    //    installed app finds the bundled jar regardless of its working dir.
    let server_jar = config.resolve_server_jar();
    if let Err(e) = cmd.run(&adb::push_server_args(&selector, &server_jar)) {
        report(status, PipelineStatus::Error(e.to_string()));
        teardown(&cmd, config);
        return Err(e);
    }
    report(status, PipelineStatus::ServerPushed);

    // 4. Set up the forward tunnel.
    if let Err(e) = cmd.run(&adb::forward_args(
        &selector,
        config.local_port,
        adb::SCRCPY_SOCKET_NAME,
    )) {
        report(status, PipelineStatus::Error(e.to_string()));
        teardown(&cmd, config);
        return Err(e);
    }
    report(status, PipelineStatus::Forwarded(config.local_port));

    // 5. Launch the scrcpy server.
    let mut server_child = match cmd.spawn(&adb::server_launch_args(&selector, &config.server_opts))
    {
        Ok(child) => child,
        Err(e) => {
            report(status, PipelineStatus::Error(e.to_string()));
            teardown(&cmd, config);
            return Err(e);
        }
    };
    report(status, PipelineStatus::ServerLaunched);

    // 6. Connect the forwarded socket(s) and run the scrcpy stream.
    let result = run_stream(config, status, frame_sink, control_sink.as_deref());

    // Teardown: kill the server child and remove the forward.
    let _ = server_child.kill();
    let _ = server_child.wait();
    teardown(&cmd, config);
    report(status, PipelineStatus::Stopped);

    result
}

/// Connect the forwarded local socket(s) and pump scrcpy frames to the sink.
///
/// This is the SHIPPED v1 video path. The scrcpy server does NOT speak the
/// FluxBridge 0xFB protocol on its sockets, so this connects a plain
/// [`std::net::TcpStream`] to `127.0.0.1:local_port` and drives
/// [`crate::scrcpy::stream_video`] (dummy byte, video header, length-prefixed
/// H.264 access units) rather than [`crate::connection::Connection`] /
/// [`crate::decode_loop::run_decode_loop`], which remain for a future
/// FluxBridge-native server.
///
/// When the server is launched with `control=true` (which the launch args
/// always set), scrcpy accepts a SECOND connection on the same forwarded socket
/// for the control channel. This opens that control socket and builds a
/// [`crate::scrcpy::ScrcpyControlSink`] against it, sized to the device
/// resolution from the parsed video header, so remote mouse/key/touch injection
/// is a live, reachable path. The sink is returned through `control_sink` (when
/// provided) so the UI input layer can drive it.
fn run_stream(
    config: &PipelineConfig,
    status: &Sender<PipelineStatus>,
    frame_sink: Option<Sender<fluxbridge_core::DecodedFrame>>,
    control_sink: Option<&ControlSinkSlot>,
) -> Result<(), adb::AdbError> {
    use std::net::TcpStream;
    use std::sync::mpsc;

    let addr = format!("{}:{}", crate::connection::DEFAULT_HOST, config.local_port);

    // First connection on the forwarded socket is the video stream.
    let mut video = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            report(status, PipelineStatus::Error(format!("video socket {addr}: {e}")));
            return Ok(());
        }
    };
    let _ = video.set_read_timeout(Some(std::time::Duration::from_secs(30)));

    // Second connection is the scrcpy control channel (opened because the
    // server is launched with control=true). A failure here is not fatal to
    // mirroring: video can still stream without input injection, so it is
    // reported but does not abort.
    let control = match TcpStream::connect(&addr) {
        Ok(s) => Some(s),
        Err(e) => {
            report(
                status,
                PipelineStatus::Error(format!("control socket {addr}: {e} (input disabled)")),
            );
            None
        }
    };

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
    let tunnel_forward = config.server_opts.tunnel_forward;

    // Consume the dummy byte and header up front so the control sink can be
    // sized from the real device resolution before frames start flowing.
    if tunnel_forward {
        if let Err(e) = crate::scrcpy::consume_dummy_byte(&mut video) {
            report(status, PipelineStatus::Error(format!("scrcpy dummy byte: {e}")));
            return Ok(());
        }
    }
    let header = match crate::scrcpy::parse_video_header(&mut video) {
        Ok(h) => h,
        Err(e) => {
            report(status, PipelineStatus::Error(format!("scrcpy video header: {e}")));
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

    if let Err(e) = crate::scrcpy::stream_av_packets(&mut video, &mut decoder, &sink) {
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

/// Best-effort teardown: remove the adb forward and (for Wi-Fi) disconnect.
///
/// Called on both the success and failure paths. Errors are ignored because
/// teardown must not mask the original outcome.
pub fn teardown(cmd: &AdbCommand, config: &PipelineConfig) {
    let selector = config.selector();
    let _ = cmd.run(&adb::forward_remove_args(&selector, config.local_port));
    if let Transport::WiFi { ip, port } = &config.transport {
        let _ = cmd.run(&adb::disconnect_args(ip, *port));
    }
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
}
