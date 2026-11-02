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
//! scrcpy server protocol version [`SCRCPY_SERVER_VERSION`]. The jar itself is
//! obtained and bundled on the Windows packaging side (see
//! `android-server/README.md`); it is deliberately NOT vendored into this
//! repository. The Kotlin skeleton under `android-server/` is retained only as
//! documentation of the equivalent `app_process` technique and is not the
//! shipped server.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};

/// The scrcpy server version the client is written against.
///
/// The pushed `scrcpy-server` jar must match this: the launch command passes it
/// as the first positional argument to
/// `com.genymobile.scrcpy.Server`, and scrcpy aborts if the version string does
/// not match the jar it was built from.
pub const SCRCPY_SERVER_VERSION: &str = "2.4";

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
                "2.4",
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
                "2.4",
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
