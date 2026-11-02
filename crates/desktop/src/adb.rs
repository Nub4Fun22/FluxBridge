//! Pure-Rust adb command construction and runtime discovery.
//!
//! FluxBridge drives Android devices through the Android Debug Bridge (`adb`)
//! command-line tool, the same way scrcpy does. This module is split into two
//! concerns:
//!
//! * **Discovery**: [`locate_adb`] finds the `adb` executable at runtime by
//!   searching `PATH`, the `ANDROID_HOME` / `ANDROID_SDK_ROOT` SDK layouts, and
//!   a handful of common install locations on Windows and Linux. It returns a
//!   typed [`AdbError::NotFound`] when nothing is found.
//! * **Command construction** (the testable core): a set of PURE functions that
//!   build `Vec<String>` argument vectors for the adb sub-commands FluxBridge
//!   needs (`push`, `forward`, `reverse`, `tcpip`, `connect`, and the
//!   `app_process` server launch). These never spawn a process, so they are
//!   unit-tested headlessly with exact-argument assertions and require no real
//!   adb or device.
//!
//! Process spawning lives in thin wrappers ([`AdbCommand::spawn`] /
//! [`AdbCommand::run`]) that simply hand the argument vector to
//! [`std::process::Command`]; all the logic that could be wrong (argument
//! ordering, the `-s SERIAL` selector, the `localabstract:` tunnel name, the
//! `CLASSPATH=... app_process` incantation) is in the pure builders.
//!
//! ## Server bootstrap decision (v1)
//!
//! FluxBridge v1 does NOT ship a bespoke Android server. It drives the upstream
//! MIT-licensed `scrcpy-server` jar: the jar is pushed to
//! `/data/local/tmp/scrcpy-server.jar` and launched with `app_process` under
//! the shell UID, exactly as the scrcpy project does. The client targets
//! scrcpy server protocol version [`SCRCPY_SERVER_VERSION`] (5.0.1). The jar itself is
//! obtained and bundled on the Windows packaging side (see
//! `android-server/README.md`); it is deliberately NOT vendored into this
//! repository. The Kotlin skeleton under `android-server/` is retained only as
//! documentation of the equivalent `app_process` technique and is not the
//! shipped server.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// The scrcpy server version the client is written against.
///
/// The pushed `scrcpy-server` jar must match this: the launch command passes it
/// as the first positional argument to
/// `com.genymobile.scrcpy.Server`, and scrcpy aborts if the version string does
/// not match the jar it was built from.
///
/// Pinned to scrcpy 5.0.1 (latest stable, supports Android 17). The client's
/// video-header and packet-flag parsing in [`crate::scrcpy`] follows the 5.0.1
/// wire format. `.github/workflows/release.yml` (`SCRCPY_VERSION`) must stay in
/// lockstep with this value so the fetched jar matches.
pub const SCRCPY_SERVER_VERSION: &str = "5.0.1";

/// On-device path the scrcpy server jar is pushed to.
pub const DEVICE_SERVER_PATH: &str = "/data/local/tmp/scrcpy-server.jar";

/// The scrcpy control/video tunnel name. scrcpy exposes its sockets as a
/// `localabstract` unix socket named `scrcpy`, which adb forwards/reverses.
pub const SCRCPY_SOCKET_NAME: &str = "scrcpy";

/// Fully-qualified name of the scrcpy server entry-point class.
pub const SCRCPY_SERVER_CLASS: &str = "com.genymobile.scrcpy.Server";

/// Errors produced by adb discovery and process wrappers.
#[derive(Debug)]
pub enum AdbError {
    /// The `adb` executable could not be located anywhere.
    NotFound {
        /// Human-readable list of the locations that were searched.
        searched: Vec<String>,
    },
    /// Spawning or waiting on an adb process failed.
    Io(std::io::Error),
    /// adb ran but exited non-zero.
    Command {
        /// The arguments that were passed to adb.
        args: Vec<String>,
        /// The process exit code, if any.
        code: Option<i32>,
        /// Captured standard error.
        stderr: String,
    },
}

impl std::fmt::Display for AdbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdbError::NotFound { searched } => write!(
                f,
                "could not locate the `adb` executable. Install Android platform-tools \
                 and ensure adb is on PATH or set ANDROID_HOME / ANDROID_SDK_ROOT. \
                 Searched: {}",
                searched.join(", ")
            ),
            AdbError::Io(e) => write!(f, "adb process error: {e}"),
            AdbError::Command { args, code, stderr } => write!(
                f,
                "adb {} failed (exit {}): {}",
                args.join(" "),
                code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                stderr.trim()
            ),
        }
    }
}

impl std::error::Error for AdbError {}

impl From<std::io::Error> for AdbError {
    fn from(e: std::io::Error) -> Self {
        AdbError::Io(e)
    }
}

/// How the device is reached, which controls whether a `-s SERIAL` selector is
/// inserted into every adb invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSelector {
    /// A single USB device; no `-s` selector is added so adb picks the only
    /// attached device.
    UsbDefault,
    /// Address or serial the invocation targets with `-s <serial>`. For Wi-Fi
    /// this is the `ip:port` string returned by `adb connect`.
    Serial(String),
}

impl DeviceSelector {
    /// The leading `-s SERIAL` argument pair, or an empty slice for
    /// [`DeviceSelector::UsbDefault`].
    fn prefix(&self) -> Vec<String> {
        match self {
            DeviceSelector::UsbDefault => Vec::new(),
            DeviceSelector::Serial(s) => vec!["-s".to_string(), s.clone()],
        }
    }
}

/// Build the argument vector for `adb [-s SERIAL] push <local> <remote>`.
pub fn push_args(sel: &DeviceSelector, local: &str, remote: &str) -> Vec<String> {
    let mut args = sel.prefix();
    args.push("push".into());
    args.push(local.into());
    args.push(remote.into());
    args
}

/// Build the argument vector to push the scrcpy server jar to the device.
pub fn push_server_args(sel: &DeviceSelector, local_jar: &str) -> Vec<String> {
    push_args(sel, local_jar, DEVICE_SERVER_PATH)
}

/// Build `adb [-s SERIAL] forward tcp:<local_port> localabstract:<name>`.
///
/// This is the USB (and forward-mode Wi-Fi) tunnel: the desktop connects to
/// `127.0.0.1:local_port` and adb relays to the device-side abstract socket.
pub fn forward_args(sel: &DeviceSelector, local_port: u16, socket_name: &str) -> Vec<String> {
    let mut args = sel.prefix();
    args.push("forward".into());
    args.push(format!("tcp:{local_port}"));
    args.push(format!("localabstract:{socket_name}"));
    args
}

/// Build `adb [-s SERIAL] forward --remove tcp:<local_port>` for teardown.
pub fn forward_remove_args(sel: &DeviceSelector, local_port: u16) -> Vec<String> {
    let mut args = sel.prefix();
    args.push("forward".into());
    args.push("--remove".into());
    args.push(format!("tcp:{local_port}"));
    args
}

/// Build `adb [-s SERIAL] reverse localabstract:<name> tcp:<local_port>`.
///
/// The alternative to [`forward_args`]: the device connects out to the desktop.
/// scrcpy uses reverse tunneling by default because it avoids a race where the
/// server socket may not be listening yet; FluxBridge supports both.
pub fn reverse_args(sel: &DeviceSelector, socket_name: &str, local_port: u16) -> Vec<String> {
    let mut args = sel.prefix();
    args.push("reverse".into());
    args.push(format!("localabstract:{socket_name}"));
    args.push(format!("tcp:{local_port}"));
    args
}

/// Build `adb [-s SERIAL] reverse --remove localabstract:<name>` for teardown.
pub fn reverse_remove_args(sel: &DeviceSelector, socket_name: &str) -> Vec<String> {
    let mut args = sel.prefix();
    args.push("reverse".into());
    args.push("--remove".into());
    args.push(format!("localabstract:{socket_name}"));
    args
}

/// Build `adb [-s SERIAL] tcpip <port>` to switch a USB device into TCP/IP mode
/// so it can subsequently be reached over Wi-Fi with [`connect_args`].
pub fn tcpip_args(sel: &DeviceSelector, port: u16) -> Vec<String> {
    let mut args = sel.prefix();
    args.push("tcpip".into());
    args.push(port.to_string());
    args
}

/// Build `adb connect <ip:port>` to attach to a device over Wi-Fi.
///
/// `connect` never takes a `-s` selector (it is what *creates* the serial), so
/// this does not use [`DeviceSelector`].
pub fn connect_args(ip: &str, port: u16) -> Vec<String> {
    vec!["connect".into(), format!("{ip}:{port}")]
}

/// Build `adb disconnect <ip:port>` for Wi-Fi teardown.
pub fn disconnect_args(ip: &str, port: u16) -> Vec<String> {
    vec!["disconnect".into(), format!("{ip}:{port}")]
}

// ---------------------------------------------------------------------------
// Android 11+ wireless debugging (mDNS discovery + one-time-code pairing).
//
// Modern Android (11+) exposes "Wireless debugging" which advertises two mDNS
// services on the LAN: a PAIRING endpoint (`_adb-tls-pairing._tcp`, used once
// with the 6-digit code the phone shows) and a CONNECT endpoint
// (`_adb-tls-connect._tcp`, used to attach after pairing and on every
// reconnect). Both ports are DYNAMIC, so FluxBridge discovers them by driving
// adb's own mDNS browser (`adb mdns services`) and parsing its text output
// rather than shipping a second mDNS stack. The arg builders below are pure
// and exact-argument tested; the parser is pure and tested against a captured
// sample. The side-effecting pair/connect orchestration lives in
// [`pair_and_connect`] / [`connect_discovered`].
// ---------------------------------------------------------------------------

/// Build `adb pair <host>:<port> <code>` for the ONE-TIME wireless pairing.
///
/// `host:port` is the `_adb-tls-pairing._tcp` endpoint discovered via
/// [`parse_mdns_services`]; `code` is the 6-digit code the phone shows under
/// Settings > Developer options > Wireless debugging > Pair device with pairing
/// code. This step cannot be fully automated because the code is only visible
/// on the phone; the user enters it once per device. `pair` never takes a `-s`
/// selector (there is no serial yet), so it does not use [`DeviceSelector`].
pub fn pair_args(host: &str, port: u16, code: &str) -> Vec<String> {
    vec!["pair".into(), format!("{host}:{port}"), code.into()]
}

/// Build `adb connect <host>:<port>` for an mDNS-discovered connect endpoint.
///
/// This is the same adb sub-command as [`connect_args`] but names its inputs
/// `host`/`port` to make clear the port is the DYNAMIC `_adb-tls-connect._tcp`
/// port discovered via mDNS (not the legacy fixed 5555 used after `adb tcpip`).
/// `connect` is what creates the serial, so it takes no `-s` selector.
pub fn connect_host_args(host: &str, port: u16) -> Vec<String> {
    vec!["connect".into(), format!("{host}:{port}")]
}

/// Build `adb mdns services` to list the wireless-debugging services adb has
/// discovered on the LAN. The text output is parsed by [`parse_mdns_services`].
pub fn mdns_services_args() -> Vec<String> {
    vec!["mdns".into(), "services".into()]
}

/// Build `adb mdns check` to report whether adb's mDNS backend is available.
pub fn mdns_check_args() -> Vec<String> {
    vec!["mdns".into(), "check".into()]
}

/// The mDNS service type of a discovered wireless-debugging endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdnsServiceKind {
    /// `_adb-tls-pairing._tcp`: the ONE-TIME pairing endpoint (used with the
    /// 6-digit code the phone shows).
    Pairing,
    /// `_adb-tls-connect._tcp`: the connect endpoint (used to attach after
    /// pairing and on every subsequent reconnect).
    Connect,
    /// Any other advertised service type adb reported.
    Other,
}

/// mDNS service type string adb prints for the pairing endpoint.
pub const MDNS_PAIRING_TYPE: &str = "_adb-tls-pairing._tcp";
/// mDNS service type string adb prints for the connect endpoint.
pub const MDNS_CONNECT_TYPE: &str = "_adb-tls-connect._tcp";

/// One service parsed out of `adb mdns services` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdnsService {
    /// The mDNS instance name, for example `adb-ABCDEF0123-AbCdEf`.
    pub instance_name: String,
    /// The service type, for example `_adb-tls-connect._tcp`.
    pub service_type: String,
    /// The resolved host (IP address) adb reported.
    pub host: String,
    /// The resolved (dynamic) port adb reported.
    pub port: u16,
}

impl MdnsService {
    /// Classify this service as a pairing endpoint, a connect endpoint, or
    /// something else, from its [`service_type`](MdnsService::service_type).
    pub fn kind(&self) -> MdnsServiceKind {
        match self.service_type.as_str() {
            MDNS_PAIRING_TYPE => MdnsServiceKind::Pairing,
            MDNS_CONNECT_TYPE => MdnsServiceKind::Connect,
            _ => MdnsServiceKind::Other,
        }
    }

    /// Whether this is a connect endpoint (`_adb-tls-connect._tcp`).
    pub fn is_connect(&self) -> bool {
        self.kind() == MdnsServiceKind::Connect
    }

    /// Whether this is a pairing endpoint (`_adb-tls-pairing._tcp`).
    pub fn is_pairing(&self) -> bool {
        self.kind() == MdnsServiceKind::Pairing
    }

    /// The `host:port` address string adb would `connect`/`pair` to.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Parse the text output of `adb mdns services` into a typed service list.
///
/// adb prints a header line followed by one line per discovered service, with
/// the instance name, the fully-qualified service type, and a `host:port`
/// address separated by whitespace (adb uses tabs, but this tolerates any run
/// of whitespace). A representative sample:
///
/// ```text
/// List of discovered mdns services
/// adb-ABCDEF0123-AbCdEf    _adb-tls-connect._tcp    192.168.1.42:37000
/// adb-ABCDEF0123-AbCdEf    _adb-tls-pairing._tcp    192.168.1.42:41355
/// ```
///
/// Parsing is tolerant: the header line is skipped, blank lines are ignored,
/// lines that do not have the three expected columns (or an unparseable port)
/// are skipped rather than erroring, and empty input yields an empty `Vec`
/// (not an error). Both the connect and pairing service types are returned;
/// callers classify them with [`MdnsService::kind`].
pub fn parse_mdns_services(output: &str) -> Vec<MdnsService> {
    let mut services = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Skip the human-readable header adb prints first.
        if line.starts_with("List of discovered") {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        // The address is the LAST column so an unexpected extra middle column
        // does not shift the host/port parsing.
        let instance_name = cols[0].to_string();
        let service_type = cols[1].to_string();
        let addr = cols[cols.len() - 1];
        let (host, port) = match addr.rsplit_once(':') {
            Some((h, p)) => match p.parse::<u16>() {
                Ok(port) => (h.to_string(), port),
                Err(_) => continue,
            },
            None => continue,
        };
        if host.is_empty() {
            continue;
        }
        services.push(MdnsService {
            instance_name,
            service_type,
            host,
            port,
        });
    }
    services
}

/// Build `adb devices -l` to list the devices adb currently has registered,
/// with the long (`-l`) format that appends `key:value` descriptors. The text
/// output is parsed by [`parse_devices`].
///
/// This is used right after a Wi-Fi `adb connect` to discover the REAL serial
/// adb registered for the device. With Android 11+ TLS wireless debugging the
/// registered serial is not reliably `ip:connect_port` (the mDNS connect port),
/// so subsequent `-s <serial>` commands must target whatever `adb devices`
/// actually lists, not the raw connect port.
pub fn devices_long_args() -> Vec<String> {
    vec!["devices".into(), "-l".into()]
}

/// One device parsed out of `adb devices -l` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbDevice {
    /// The serial adb registered the device under (a USB serial, or for Wi-Fi
    /// an `ip:port` string whose port may differ from the mDNS connect port).
    pub serial: String,
    /// The connection state adb reported: `device` (ready), `offline`,
    /// `unauthorized`, `bootloader`, and so on.
    pub state: String,
}

impl AdbDevice {
    /// Whether the device is online and ready for commands (state `device`).
    pub fn is_online(&self) -> bool {
        self.state == "device"
    }
}

/// Parse the text output of `adb devices -l` into a typed device list.
///
/// adb prints a `List of devices attached` header followed by one line per
/// device: the serial, the state, then (with `-l`) a run of `key:value`
/// descriptors such as `product:... model:... device:... transport_id:N`. A
/// representative sample:
///
/// ```text
/// List of devices attached
/// 0A1B2C3D4E5F       device product:sdk_phone model:Pixel_7 device:panther transport_id:1
/// 192.168.1.42:42817 device product:redfin model:Pixel_5 device:redfin transport_id:3
/// 192.168.1.99:5555  offline
/// ```
///
/// Parsing mirrors [`parse_mdns_services`] and is tolerant: the header is
/// skipped, blank lines are ignored, the first whitespace column is the serial
/// and the second is the state, trailing descriptors are ignored, and a line
/// without at least a serial and a state is skipped rather than erroring. Empty
/// input yields an empty `Vec` (not an error).
pub fn parse_devices(output: &str) -> Vec<AdbDevice> {
    let mut devices = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Skip the human-readable header adb prints first.
        if line.starts_with("List of devices") {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        // A device line needs at least a serial and a state; anything shorter
        // (a stray informational line) is skipped rather than errored.
        if cols.len() < 2 {
            continue;
        }
        let serial = cols[0].to_string();
        let state = cols[1].to_string();
        if serial.is_empty() || state.is_empty() {
            continue;
        }
        devices.push(AdbDevice { serial, state });
    }
    devices
}

/// Pick the REAL adb-registered serial for a just-connected Wi-Fi device.
///
/// After `adb connect <host>:<connect_port>` succeeds, the serial adb registers
/// in `adb devices` is NOT reliably `host:connect_port` with Android 11+ TLS
/// wireless debugging: adb may list the device under a different port. This
/// resolves the serial to thread into every subsequent `-s <serial>` command
/// (push, forward, launch, teardown) from the parsed [`AdbDevice`] list:
///
/// 1. Prefer an exact `host:connect_port` match (ideally online).
/// 2. Otherwise prefer any serial starting with `host:` (a different port adb
///    chose), preferring an online `device`-state entry.
///
/// Within each tier an online device is preferred over an offline/unauthorized
/// one so the pipeline does not latch onto a serial that is not yet ready.
/// Returns the resolved serial, or `None` when no device matching `host` is
/// present at all.
pub fn resolve_wireless_serial(
    devices: &[AdbDevice],
    host: &str,
    connect_port: u16,
) -> Option<String> {
    let exact = format!("{host}:{connect_port}");
    let host_prefix = format!("{host}:");

    // Tier 1: an exact host:connect_port match, online first.
    if let Some(d) = devices
        .iter()
        .find(|d| d.serial == exact && d.is_online())
    {
        return Some(d.serial.clone());
    }
    // Tier 2: any serial for this host (any port), online first.
    if let Some(d) = devices
        .iter()
        .find(|d| d.serial.starts_with(&host_prefix) && d.is_online())
    {
        return Some(d.serial.clone());
    }
    // Tier 3: fall back to a matching serial even if not yet online, so the
    // caller can surface an actionable offline/unauthorized error rather than
    // a bare "not found". Exact match wins over a different-port match.
    if let Some(d) = devices.iter().find(|d| d.serial == exact) {
        return Some(d.serial.clone());
    }
    devices
        .iter()
        .find(|d| d.serial.starts_with(&host_prefix))
        .map(|d| d.serial.clone())
}

/// Outcome of the wireless pair-then-connect orchestration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirelessConnection {
    /// The connect endpoint host the device was reached at.
    pub host: String,
    /// The (dynamic) connect port the device was reached at.
    pub connect_port: u16,
}

impl WirelessConnection {
    /// The `host:port` serial `adb connect` created, usable as a
    /// [`DeviceSelector::Serial`].
    pub fn serial(&self) -> String {
        format!("{}:{}", self.host, self.connect_port)
    }
}

/// Options controlling the scrcpy server launch command.
#[derive(Debug, Clone)]
pub struct ServerLaunchOptions {
    /// Max video size (longest edge) in pixels; `0` means the device default.
    pub max_size: u32,
    /// Target video bitrate in bits per second.
    pub bit_rate: u32,
    /// Max frame rate; `0` means unlimited.
    pub max_fps: u32,
    /// Whether the server uses tunnel-forward (`true`) or reverse (`false`).
    pub tunnel_forward: bool,
}

impl Default for ServerLaunchOptions {
    fn default() -> Self {
        ServerLaunchOptions {
            max_size: 0,
            bit_rate: 8_000_000,
            max_fps: 0,
            tunnel_forward: true,
        }
    }
}

/// Build the `adb [-s SERIAL] shell CLASSPATH=... app_process / <class> <ver> ...`
/// command that launches the scrcpy server on the device under the shell UID.
///
/// The positional/`key=value` arguments after the class name follow the scrcpy
/// server 2.x command line: the first positional is the version string (checked
/// against the jar), followed by `key=value` options.
pub fn server_launch_args(
    sel: &DeviceSelector,
    opts: &ServerLaunchOptions,
) -> Vec<String> {
    let mut args = sel.prefix();
    args.push("shell".into());
    args.push(format!("CLASSPATH={DEVICE_SERVER_PATH}"));
    args.push("app_process".into());
    args.push("/".into());
    args.push(SCRCPY_SERVER_CLASS.into());
    args.push(SCRCPY_SERVER_VERSION.into());
    args.push("log_level=info".into());
    args.push(format!("max_size={}", opts.max_size));
    args.push(format!("video_bit_rate={}", opts.bit_rate));
    args.push(format!("max_fps={}", opts.max_fps));
    args.push(format!(
        "tunnel_forward={}",
        if opts.tunnel_forward { "true" } else { "false" }
    ));
    args.push("audio=false".into());
    args.push("control=true".into());
    args
}

/// A located adb executable plus the device selector to use with it.
#[derive(Debug, Clone)]
pub struct AdbCommand {
    /// Absolute path to the resolved `adb` executable.
    pub adb_path: PathBuf,
    /// The device this command targets.
    pub selector: DeviceSelector,
}

impl AdbCommand {
    /// Locate adb on the host and bind it to `selector`.
    pub fn locate(selector: DeviceSelector) -> Result<Self, AdbError> {
        Ok(AdbCommand {
            adb_path: locate_adb()?,
            selector,
        })
    }

    /// Create a command from an explicit adb path (used by tests / overrides).
    pub fn with_path(adb_path: PathBuf, selector: DeviceSelector) -> Self {
        AdbCommand { adb_path, selector }
    }

    /// Spawn adb with `args` and return the child without waiting (used for the
    /// long-lived server launch).
    pub fn spawn(&self, args: &[String]) -> Result<Child, AdbError> {
        Command::new(&self.adb_path)
            .args(args)
            .spawn()
            .map_err(AdbError::Io)
    }

    /// Spawn adb with `args` capturing the child's stdout AND stderr as pipes,
    /// returning the child without waiting.
    ///
    /// This is used for the long-lived scrcpy server launch. The scrcpy server
    /// logs to stderr (a version mismatch, an `app_process` failure, an
    /// unsupported option, or a Java stack trace all print there), and some
    /// messages may go to stdout, so both are piped. With the default
    /// [`AdbCommand::spawn`] the child inherits the parent's stdio and that
    /// output goes nowhere, which is exactly why a server that dies at startup
    /// surfaces only the opaque "dummy byte: failed to fill whole buffer" read
    /// error. The caller is responsible for draining the pipes on a background
    /// thread so a chatty or silent server never blocks the pipeline (see
    /// [`crate::pipeline`]).
    pub fn spawn_capturing(&self, args: &[String]) -> Result<Child, AdbError> {
        Command::new(&self.adb_path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(AdbError::Io)
    }

    /// Run adb with `args` to completion, erroring on a non-zero exit.
    pub fn run(&self, args: &[String]) -> Result<Output, AdbError> {
        let output = Command::new(&self.adb_path)
            .args(args)
            .output()
            .map_err(AdbError::Io)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(AdbError::Command {
                args: args.to_vec(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    /// List the wireless-debugging services adb has discovered on the LAN.
    ///
    /// Runs `adb mdns services` and parses the text output with
    /// [`parse_mdns_services`]. This is a thin side-effecting wrapper over
    /// [`AdbCommand::run`]; the parsing (the part that can be wrong) is pure
    /// and unit-tested. A returning empty list means nothing was discovered
    /// (the phone's Wireless debugging screen may not be open), which is not an
    /// error.
    pub fn mdns_services(&self) -> Result<Vec<MdnsService>, AdbError> {
        let output = self.run(&mdns_services_args())?;
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_mdns_services(&text))
    }

    /// Pair this device ONCE using the 6-digit code shown on the phone.
    ///
    /// Runs `adb pair <host>:<pair_port> <code>` against the pairing endpoint
    /// discovered via mDNS. Interprets the result: adb exits non-zero and this
    /// returns [`AdbError::Command`] on a wrong/expired code or an unreachable
    /// endpoint; some adb builds exit zero but print `failed`/`error` to
    /// stdout, which is also treated as a failure so the UI never reports a
    /// false success. The 6-digit `code` MUST come from the user (it is only
    /// visible on the phone), so this step is inherently not zero-touch.
    pub fn pair(&self, host: &str, pair_port: u16, code: &str) -> Result<(), AdbError> {
        let args = pair_args(host, pair_port, code);
        let output = self.run(&args)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lower = stdout.to_lowercase();
        if lower.contains("successfully paired") {
            return Ok(());
        }
        if lower.contains("failed") || lower.contains("error") {
            return Err(AdbError::Command {
                args,
                code: output.status.code(),
                stderr: stdout.trim().to_string(),
            });
        }
        // No explicit success/failure marker in stdout. We deliberately TRUST
        // adb's zero exit code here and report success: `run()` already rejected
        // every non-zero exit, so reaching this point means adb itself
        // considered the pair command successful. The assumption is that a
        // genuine pairing failure either exits non-zero or prints a
        // `failed`/`error` marker (both handled above); a locale or future adb
        // wording that exits zero yet omits the "successfully paired" marker
        // would fall through here and be treated as paired.
        Ok(())
    }

    /// Attach to a device at an mDNS-discovered connect endpoint.
    ///
    /// Runs `adb connect <host>:<connect_port>`. adb exits zero even for some
    /// failures and prints the outcome to stdout, so this inspects stdout for a
    /// `connected`/`already connected` marker and returns [`AdbError::Command`]
    /// otherwise. On success it returns the [`WirelessConnection`] carrying the
    /// `host:connect_port` serial the mirroring pipeline targets.
    pub fn connect_discovered(
        &self,
        host: &str,
        connect_port: u16,
    ) -> Result<WirelessConnection, AdbError> {
        let args = connect_host_args(host, connect_port);
        let output = self.run(&args)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let lower = stdout.to_lowercase();
        if lower.contains("connected to") || lower.contains("already connected") {
            Ok(WirelessConnection {
                host: host.to_string(),
                connect_port,
            })
        } else {
            Err(AdbError::Command {
                args,
                code: output.status.code(),
                stderr: stdout.trim().to_string(),
            })
        }
    }

    /// Resolve the REAL adb-registered serial for a device just connected over
    /// Wi-Fi at `host:connect_port`.
    ///
    /// Runs `adb devices -l`, parses it with [`parse_devices`], and uses
    /// [`resolve_wireless_serial`] to find the serial adb actually registered
    /// (which, with Android 11+ TLS wireless debugging, is not reliably
    /// `host:connect_port`). adb lags for a moment after `connect`, so this
    /// retries a SHORT bounded number of times until the matching device is
    /// present AND online; the retry/sleep is the only impure part, while the
    /// parsing and resolution are pure and separately unit-tested.
    ///
    /// Returns the resolved serial on success. If a matching serial appears but
    /// never reaches the `device` state within the retry budget, returns an
    /// actionable [`AdbError::Command`] telling the user to accept the
    /// debugging prompt (offline/unauthorized). If no device matching `host` is
    /// ever listed, returns an [`AdbError::Command`] saying the connected
    /// device was not found in `adb devices`.
    pub fn resolve_connected_serial(
        &self,
        host: &str,
        connect_port: u16,
    ) -> Result<String, AdbError> {
        /// How many times to poll `adb devices -l` before giving up.
        const RESOLVE_ATTEMPTS: u32 = 5;
        /// Delay between polls; adb lags briefly after `connect`.
        const RESOLVE_SLEEP: std::time::Duration = std::time::Duration::from_millis(300);

        let args = devices_long_args();
        let mut last_match: Option<String> = None;
        for attempt in 0..RESOLVE_ATTEMPTS {
            let output = self.run(&args)?;
            let text = String::from_utf8_lossy(&output.stdout);
            let devices = parse_devices(&text);
            if let Some(serial) = resolve_wireless_serial(&devices, host, connect_port) {
                // Online and matching: done.
                if devices
                    .iter()
                    .any(|d| d.serial == serial && d.is_online())
                {
                    return Ok(serial);
                }
                // Present but not yet online; remember it so a terminal
                // offline/unauthorized state yields an actionable error.
                last_match = Some(serial);
            }
            if attempt + 1 < RESOLVE_ATTEMPTS {
                std::thread::sleep(RESOLVE_SLEEP);
            }
        }

        match last_match {
            Some(serial) => Err(AdbError::Command {
                args,
                code: None,
                stderr: format!(
                    "device {serial} is offline or unauthorized; accept the \
                     wireless debugging prompt on the phone and try again"
                ),
            }),
            None => Err(AdbError::Command {
                args,
                code: None,
                stderr: format!(
                    "connected device {host}:{connect_port} was not found in \
                     `adb devices`; the connection may have dropped"
                ),
            }),
        }
    }

    /// The full one-time wireless onboarding: pair with the 6-digit code, then
    /// connect. Returns the [`WirelessConnection`] the pipeline uses as its
    /// Wi-Fi transport. Pairing and connecting may use different (dynamic)
    /// ports, both discovered via mDNS.
    pub fn pair_and_connect(
        &self,
        host: &str,
        pair_port: u16,
        connect_port: u16,
        code: &str,
    ) -> Result<WirelessConnection, AdbError> {
        self.pair(host, pair_port, code)?;
        self.connect_discovered(host, connect_port)
    }
}

/// The adb executable file name for the current OS.
fn adb_exe_name() -> &'static str {
    if cfg!(windows) {
        "adb.exe"
    } else {
        "adb"
    }
}

/// Locate the `adb` executable at runtime.
///
/// Search order:
/// 1. `ANDROID_HOME` / `ANDROID_SDK_ROOT` + `platform-tools/adb[.exe]`.
/// 2. Every directory on `PATH` (via `PATH` + the OS exe name).
/// 3. Common install locations for the host OS.
///
/// Returns [`AdbError::NotFound`] listing the searched locations when adb is not
/// present. This runs on the user's machine at connect time, never at build
/// time, so the headless CI host (which has no adb) is unaffected.
pub fn locate_adb() -> Result<PathBuf, AdbError> {
    let mut searched: Vec<String> = Vec::new();
    let exe = adb_exe_name();

    // 0. A previously auto-provisioned bundled adb (see
    //    `crate::provision`). Once FluxBridge has downloaded platform-tools into
    //    the per-user data dir, that copy is preferred so adb is found on every
    //    subsequent run without re-downloading and without depending on the
    //    user having installed an SDK or edited PATH.
    if let Ok(data_dir) = crate::provision::data_dir() {
        let candidate = crate::provision::provisioned_adb_path(&data_dir);
        searched.push(candidate.display().to_string());
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    // 1. Android SDK environment variables.
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = std::env::var_os(var) {
            let candidate = Path::new(&root).join("platform-tools").join(exe);
            searched.push(candidate.display().to_string());
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    // 2. PATH entries.
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        searched.push(format!("PATH ({} entries)", std::env::split_paths(&path).count()));
    }

    // 3. Common fixed install locations.
    for candidate in common_install_locations() {
        searched.push(candidate.display().to_string());
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(AdbError::NotFound { searched })
}

/// Locate adb, preferring a bundled copy already provisioned under `data_dir`.
///
/// This is the entry point the pipeline uses so the lookup is deterministic and
/// testable with an explicit data dir: if `<data_dir>/platform-tools/adb[.exe]`
/// exists it is returned immediately, otherwise the lookup falls back to the
/// full [`locate_adb`] search order (SDK env vars, PATH, fixed locations). When
/// everything fails the caller can auto-provision adb into `data_dir` via
/// [`crate::provision::ensure_adb`] and call again.
pub fn locate_or_provisioned(data_dir: &Path) -> Result<PathBuf, AdbError> {
    let provisioned = crate::provision::provisioned_adb_path(data_dir);
    if provisioned.is_file() {
        return Ok(provisioned);
    }
    locate_adb()
}

/// Common fixed locations adb is installed to, per OS.
fn common_install_locations() -> Vec<PathBuf> {
    let exe = adb_exe_name();
    let mut out = Vec::new();
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            out.push(
                Path::new(&local)
                    .join("Android")
                    .join("Sdk")
                    .join("platform-tools")
                    .join(exe),
            );
        }
        out.push(PathBuf::from("C:\\platform-tools").join(exe));
    } else {
        if let Some(home) = std::env::var_os("HOME") {
            out.push(
                Path::new(&home)
                    .join("Android")
                    .join("Sdk")
                    .join("platform-tools")
                    .join(exe),
            );
            out.push(
                Path::new(&home)
                    .join("Library")
                    .join("Android")
                    .join("sdk")
                    .join("platform-tools")
                    .join(exe),
            );
        }
        out.push(PathBuf::from("/usr/local/bin").join(exe));
        out.push(PathBuf::from("/usr/bin").join(exe));
        out.push(PathBuf::from("/opt/platform-tools").join(exe));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_server_args_usb() {
        let args = push_server_args(&DeviceSelector::UsbDefault, "scrcpy-server.jar");
        assert_eq!(
            args,
            vec!["push", "scrcpy-server.jar", "/data/local/tmp/scrcpy-server.jar"]
        );
    }

    #[test]
    fn push_server_args_wifi_serial() {
        let sel = DeviceSelector::Serial("192.168.1.42:5555".into());
        let args = push_server_args(&sel, "scrcpy-server.jar");
        assert_eq!(
            args,
            vec![
                "-s",
                "192.168.1.42:5555",
                "push",
                "scrcpy-server.jar",
                "/data/local/tmp/scrcpy-server.jar"
            ]
        );
    }

    #[test]
    fn forward_args_usb_uses_localabstract_scrcpy() {
        let args = forward_args(&DeviceSelector::UsbDefault, 27183, SCRCPY_SOCKET_NAME);
        assert_eq!(
            args,
            vec!["forward", "tcp:27183", "localabstract:scrcpy"]
        );
    }

    #[test]
    fn forward_args_wifi_inserts_serial_selector() {
        let sel = DeviceSelector::Serial("192.168.1.42:5555".into());
        let args = forward_args(&sel, 27183, SCRCPY_SOCKET_NAME);
        assert_eq!(
            args,
            vec![
                "-s",
                "192.168.1.42:5555",
                "forward",
                "tcp:27183",
                "localabstract:scrcpy"
            ]
        );
    }

    #[test]
    fn forward_remove_args_usb() {
        let args = forward_remove_args(&DeviceSelector::UsbDefault, 27183);
        assert_eq!(args, vec!["forward", "--remove", "tcp:27183"]);
    }

    #[test]
    fn reverse_args_usb() {
        let args = reverse_args(&DeviceSelector::UsbDefault, SCRCPY_SOCKET_NAME, 27183);
        assert_eq!(
            args,
            vec!["reverse", "localabstract:scrcpy", "tcp:27183"]
        );
    }

    #[test]
    fn reverse_remove_args_usb() {
        let args = reverse_remove_args(&DeviceSelector::UsbDefault, SCRCPY_SOCKET_NAME);
        assert_eq!(args, vec!["reverse", "--remove", "localabstract:scrcpy"]);
    }

    #[test]
    fn tcpip_args_builds_port() {
        let args = tcpip_args(&DeviceSelector::UsbDefault, 5555);
        assert_eq!(args, vec!["tcpip", "5555"]);
    }

    #[test]
    fn connect_and_disconnect_args() {
        assert_eq!(
            connect_args("192.168.1.42", 5555),
            vec!["connect", "192.168.1.42:5555"]
        );
        assert_eq!(
            disconnect_args("192.168.1.42", 5555),
            vec!["disconnect", "192.168.1.42:5555"]
        );
    }

    #[test]
    fn pair_args_exact() {
        assert_eq!(
            pair_args("192.168.1.42", 41355, "123456"),
            vec!["pair", "192.168.1.42:41355", "123456"]
        );
    }

    #[test]
    fn connect_host_args_uses_discovered_dynamic_port() {
        // The mDNS connect port is dynamic (not the legacy 5555); the builder
        // uses exactly the discovered host:port.
        assert_eq!(
            connect_host_args("192.168.1.42", 37000),
            vec!["connect", "192.168.1.42:37000"]
        );
    }

    #[test]
    fn mdns_services_and_check_args_exact() {
        assert_eq!(mdns_services_args(), vec!["mdns", "services"]);
        assert_eq!(mdns_check_args(), vec!["mdns", "check"]);
    }

    #[test]
    fn parse_mdns_services_captured_sample_distinguishes_connect_and_pairing() {
        // Representative captured `adb mdns services` output: a header line,
        // then a connect and a pairing service for the same device, with
        // tab-separated columns and DYNAMIC ports.
        let sample = "List of discovered mdns services\n\
            adb-ABCDEF0123-AbCdEf\t_adb-tls-connect._tcp\t192.168.1.42:37000\n\
            adb-ABCDEF0123-AbCdEf\t_adb-tls-pairing._tcp\t192.168.1.42:41355\n";
        let services = parse_mdns_services(sample);
        assert_eq!(services.len(), 2, "two services parsed: {services:?}");

        let connect = services
            .iter()
            .find(|s| s.is_connect())
            .expect("a connect service");
        assert_eq!(connect.instance_name, "adb-ABCDEF0123-AbCdEf");
        assert_eq!(connect.service_type, "_adb-tls-connect._tcp");
        assert_eq!(connect.host, "192.168.1.42");
        assert_eq!(connect.port, 37000);
        assert_eq!(connect.kind(), MdnsServiceKind::Connect);
        assert_eq!(connect.address(), "192.168.1.42:37000");

        let pairing = services
            .iter()
            .find(|s| s.is_pairing())
            .expect("a pairing service");
        assert_eq!(pairing.service_type, "_adb-tls-pairing._tcp");
        assert_eq!(pairing.port, 41355);
        assert_eq!(pairing.kind(), MdnsServiceKind::Pairing);
    }

    #[test]
    fn parse_mdns_services_tolerates_space_separation_and_no_header() {
        // Some adb builds emit no header and space-separated columns; still
        // parsed, and an unknown service type classifies as Other.
        let sample = "adb-XYZ999 _adb-tls-connect._tcp 10.0.0.5:44000\n\
            adb-XYZ999 _unrelated._tcp 10.0.0.5:9\n";
        let services = parse_mdns_services(sample);
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].host, "10.0.0.5");
        assert_eq!(services[0].port, 44000);
        assert_eq!(services[0].kind(), MdnsServiceKind::Connect);
        assert_eq!(services[1].kind(), MdnsServiceKind::Other);
    }

    #[test]
    fn parse_mdns_services_empty_output_is_empty_vec_not_error() {
        assert!(parse_mdns_services("").is_empty());
        assert!(parse_mdns_services("List of discovered mdns services\n").is_empty());
        // A malformed line (missing the address column, unparseable port) is
        // skipped rather than panicking or erroring.
        assert!(parse_mdns_services("adb-bad\t_adb-tls-connect._tcp\n").is_empty());
        assert!(parse_mdns_services("adb-bad\t_adb-tls-connect._tcp\t1.2.3.4:notaport").is_empty());
    }

    #[test]
    fn devices_long_args_exact() {
        assert_eq!(devices_long_args(), vec!["devices", "-l"]);
    }

    #[test]
    fn parse_devices_captured_sample_usb_and_wireless() {
        // Representative captured `adb devices -l` output: the header, a USB
        // device, and a wireless device whose registered port differs from any
        // mDNS connect port, each with trailing key:value descriptors. adb uses
        // a run of spaces for alignment, which the whitespace split tolerates.
        let sample = "List of devices attached\n\
            0A1B2C3D4E5F       device product:panther model:Pixel_7 device:panther transport_id:1\n\
            192.168.1.42:42817 device product:redfin model:Pixel_5 device:redfin transport_id:3\n";
        let devices = parse_devices(sample);
        assert_eq!(devices.len(), 2, "two devices parsed: {devices:?}");

        assert_eq!(devices[0].serial, "0A1B2C3D4E5F");
        assert_eq!(devices[0].state, "device");
        assert!(devices[0].is_online());

        assert_eq!(devices[1].serial, "192.168.1.42:42817");
        assert_eq!(devices[1].state, "device");
        assert!(devices[1].is_online());
    }

    #[test]
    fn parse_devices_captures_offline_and_unauthorized_states() {
        let sample = "List of devices attached\n\
            192.168.1.99:5555  offline\n\
            9C8B7A6D           unauthorized\n";
        let devices = parse_devices(sample);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].state, "offline");
        assert!(!devices[0].is_online());
        assert_eq!(devices[1].state, "unauthorized");
        assert!(!devices[1].is_online());
    }

    #[test]
    fn parse_devices_empty_and_header_only_and_malformed_are_safe() {
        assert!(parse_devices("").is_empty());
        assert!(parse_devices("List of devices attached\n").is_empty());
        // A line with only a serial and no state is skipped, not errored.
        assert!(parse_devices("List of devices attached\nloneserial\n").is_empty());
    }

    #[test]
    fn parse_devices_tolerates_tab_separation() {
        // Some outputs use tabs between the serial and state rather than the
        // aligned spaces; the whitespace split handles both.
        let sample = "List of devices attached\n0A1B2C3D4E5F\tdevice\n";
        let devices = parse_devices(sample);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].serial, "0A1B2C3D4E5F");
        assert_eq!(devices[0].state, "device");
    }

    #[test]
    fn resolve_wireless_serial_prefers_different_registered_port() {
        // The device connected on the mDNS connect port 37000, but adb
        // registered it under a different port (42817). The resolver returns
        // the registered serial, NOT the raw connect-port serial.
        let devices = vec![
            AdbDevice {
                serial: "0A1B2C3D4E5F".into(),
                state: "device".into(),
            },
            AdbDevice {
                serial: "192.168.1.42:42817".into(),
                state: "device".into(),
            },
        ];
        let resolved = resolve_wireless_serial(&devices, "192.168.1.42", 37000);
        assert_eq!(resolved.as_deref(), Some("192.168.1.42:42817"));
    }

    #[test]
    fn resolve_wireless_serial_prefers_exact_match_when_present() {
        let devices = vec![
            AdbDevice {
                serial: "192.168.1.42:37000".into(),
                state: "device".into(),
            },
            AdbDevice {
                serial: "192.168.1.42:42817".into(),
                state: "device".into(),
            },
        ];
        let resolved = resolve_wireless_serial(&devices, "192.168.1.42", 37000);
        assert_eq!(resolved.as_deref(), Some("192.168.1.42:37000"));
    }

    #[test]
    fn resolve_wireless_serial_prefers_online_over_offline() {
        // An offline exact match and an online different-port match for the same
        // host: the online one is chosen so the pipeline does not latch onto a
        // serial that is not ready.
        let devices = vec![
            AdbDevice {
                serial: "192.168.1.42:37000".into(),
                state: "offline".into(),
            },
            AdbDevice {
                serial: "192.168.1.42:42817".into(),
                state: "device".into(),
            },
        ];
        let resolved = resolve_wireless_serial(&devices, "192.168.1.42", 37000);
        assert_eq!(resolved.as_deref(), Some("192.168.1.42:42817"));
    }

    #[test]
    fn resolve_wireless_serial_returns_offline_match_when_none_online() {
        // No online candidate: the resolver still returns the matching serial
        // (so the caller can surface an actionable offline/unauthorized error),
        // preferring the exact match.
        let devices = vec![AdbDevice {
            serial: "192.168.1.42:37000".into(),
            state: "unauthorized".into(),
        }];
        let resolved = resolve_wireless_serial(&devices, "192.168.1.42", 37000);
        assert_eq!(resolved.as_deref(), Some("192.168.1.42:37000"));
    }

    #[test]
    fn resolve_wireless_serial_single_wireless_device() {
        let devices = vec![AdbDevice {
            serial: "10.0.0.5:44321".into(),
            state: "device".into(),
        }];
        assert_eq!(
            resolve_wireless_serial(&devices, "10.0.0.5", 44000).as_deref(),
            Some("10.0.0.5:44321")
        );
    }

    #[test]
    fn resolve_wireless_serial_none_when_host_absent() {
        let devices = vec![AdbDevice {
            serial: "0A1B2C3D4E5F".into(),
            state: "device".into(),
        }];
        assert!(resolve_wireless_serial(&devices, "192.168.1.42", 37000).is_none());
        assert!(resolve_wireless_serial(&[], "192.168.1.42", 37000).is_none());
    }

    #[test]
    fn wireless_connection_serial_is_host_connect_port() {
        let wc = WirelessConnection {
            host: "192.168.1.42".into(),
            connect_port: 37000,
        };
        assert_eq!(wc.serial(), "192.168.1.42:37000");
    }

    #[test]
    fn server_launch_args_usb_exact() {
        let args = server_launch_args(&DeviceSelector::UsbDefault, &ServerLaunchOptions::default());
        assert_eq!(
            args,
            vec![
                "shell",
                "CLASSPATH=/data/local/tmp/scrcpy-server.jar",
                "app_process",
                "/",
                "com.genymobile.scrcpy.Server",
                "5.0.1",
                "log_level=info",
                "max_size=0",
                "video_bit_rate=8000000",
                "max_fps=0",
                "tunnel_forward=true",
                "audio=false",
                "control=true",
            ]
        );
    }

    #[test]
    fn server_launch_args_wifi_serial_prefix_and_reverse() {
        let sel = DeviceSelector::Serial("192.168.1.42:5555".into());
        let opts = ServerLaunchOptions {
            max_size: 1280,
            bit_rate: 4_000_000,
            max_fps: 60,
            tunnel_forward: false,
        };
        let args = server_launch_args(&sel, &opts);
        assert_eq!(
            args,
            vec![
                "-s",
                "192.168.1.42:5555",
                "shell",
                "CLASSPATH=/data/local/tmp/scrcpy-server.jar",
                "app_process",
                "/",
                "com.genymobile.scrcpy.Server",
                "5.0.1",
                "log_level=info",
                "max_size=1280",
                "video_bit_rate=4000000",
                "max_fps=60",
                "tunnel_forward=false",
                "audio=false",
                "control=true",
            ]
        );
    }

    /// The client's pinned [`SCRCPY_SERVER_VERSION`] MUST be exactly the first
    /// positional argument emitted after the server class name by
    /// [`server_launch_args`]. scrcpy aborts instantly if the version string in
    /// the launch command does not match the jar it was built from, which
    /// manifests downstream as an immediate EOF on the video socket (the
    /// "dummy byte: failed to fill whole buffer" failure). Locking them
    /// together here means the pinned constant and the launch command can never
    /// silently diverge. (The workflow side of the version agreement, that the
    /// downloaded jar is the matching v5.0.1 upstream asset, is covered in the
    /// release workflow.)
    #[test]
    fn server_launch_args_version_is_first_positional_after_class() {
        for sel in [
            DeviceSelector::UsbDefault,
            DeviceSelector::Serial("192.168.1.42:37000".into()),
        ] {
            let args = server_launch_args(&sel, &ServerLaunchOptions::default());
            let class_pos = args
                .iter()
                .position(|a| a == SCRCPY_SERVER_CLASS)
                .expect("the launch command names the scrcpy server class");
            let version = args
                .get(class_pos + 1)
                .expect("a positional argument follows the server class");
            assert_eq!(
                version, SCRCPY_SERVER_VERSION,
                "the first positional after the server class must be the pinned \
                 SCRCPY_SERVER_VERSION so the client and jar versions cannot diverge"
            );
        }
    }

    /// Pin the exact scrcpy server version so an accidental revert fails loudly.
    /// The client's 5.0.1 video-header (session-flags word) and packet-flag bit
    /// parsing in [`crate::scrcpy`] depend on this being 5.0.1, and
    /// `.github/workflows/release.yml` must fetch the matching jar.
    #[test]
    fn scrcpy_server_version_is_pinned_to_5_0_1() {
        assert_eq!(SCRCPY_SERVER_VERSION, "5.0.1");
    }

    #[test]
    fn locate_adb_not_found_returns_typed_error_with_searched_list() {
        // Point the SDK vars at a directory that cannot contain adb and strip
        // PATH so discovery fails deterministically. This never spawns adb.
        temp_env_clear(|| {
            std::env::set_var("ANDROID_HOME", "/nonexistent-sdk-root-xyz");
            std::env::remove_var("ANDROID_SDK_ROOT");
            std::env::set_var("PATH", "/nonexistent-path-xyz");
            std::env::remove_var("HOME");
            std::env::remove_var("LOCALAPPDATA");
            std::env::remove_var("XDG_DATA_HOME");
            match locate_adb() {
                Err(AdbError::NotFound { searched }) => {
                    assert!(
                        searched.iter().any(|s| s.contains("nonexistent-sdk-root-xyz")),
                        "searched list should mention the SDK root: {searched:?}"
                    );
                }
                other => panic!("expected NotFound, got {other:?}"),
            }
        });
    }

    /// When a bundled adb has been provisioned under the data dir,
    /// [`locate_or_provisioned`] returns it ahead of the normal search order
    /// (which, with the SDK vars and PATH stripped, would otherwise be a
    /// NotFound). A dummy file stands in for the real adb binary.
    #[test]
    fn locate_or_provisioned_prefers_bundled_adb() {
        let data_dir = std::env::temp_dir().join(format!(
            "fluxbridge-locate-prov-{}",
            std::process::id()
        ));
        let adb_path = crate::provision::provisioned_adb_path(&data_dir);
        std::fs::create_dir_all(adb_path.parent().unwrap()).unwrap();
        std::fs::write(&adb_path, b"dummy adb").unwrap();

        temp_env_clear(|| {
            // Strip everything so the fallback search would fail; the bundled
            // copy must still be found and returned first.
            std::env::remove_var("ANDROID_HOME");
            std::env::remove_var("ANDROID_SDK_ROOT");
            std::env::set_var("PATH", "/nonexistent-path-xyz");
            std::env::remove_var("HOME");
            std::env::remove_var("LOCALAPPDATA");
            std::env::remove_var("XDG_DATA_HOME");
            let found = locate_or_provisioned(&data_dir).expect("bundled adb should be found");
            assert_eq!(found, adb_path);
        });

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// Run `f` with a saved/restored snapshot of the env vars we mutate, so the
    /// discovery test does not leak state into other tests. Delegates to the
    /// crate-wide [`crate::test_env::with_env_lock`] so it serializes through
    /// the SAME process-global lock as every other env-mutating desktop test
    /// (env vars are process-global, so independent locks would race), and
    /// saves/restores the full shared key set (including `XDG_DATA_HOME`).
    fn temp_env_clear(f: impl FnOnce()) {
        crate::test_env::with_env_lock(f);
    }
}
