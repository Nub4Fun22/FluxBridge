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

    /// Run `f` with a saved/restored snapshot of the env vars we mutate, so the
    /// discovery test does not leak state into other tests. Serialized through a
    /// mutex because env vars are process-global.
    fn temp_env_clear(f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            ["ANDROID_HOME", "ANDROID_SDK_ROOT", "PATH", "HOME", "LOCALAPPDATA"]
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
}
