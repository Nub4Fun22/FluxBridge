//! Feature-independent glue for the wireless discovery / pairing UX.
//!
//! The Slint panel (`ui.rs`, behind the `gui` feature) stays thin: it spawns a
//! worker thread, calls into the small testable pieces here, and drains typed
//! [`WirelessEvent`]s onto its status line and device list. All the logic that
//! could be wrong (adb discovery, mDNS parsing, choosing the pairing vs connect
//! endpoint, persisting the paired device) lives in feature-independent code so
//! the headless CI host builds and tests it without any Slint dependency.
//!
//! The actual wireless onboarding is the Android 11+ flow (see [`crate::adb`]):
//!
//! 1. The user enables Wireless debugging on the phone (same Wi-Fi as the PC).
//! 2. `adb mdns services` advertises a `_adb-tls-pairing._tcp` endpoint and,
//!    once paired, a `_adb-tls-connect._tcp` endpoint. Both ports are DYNAMIC.
//! 3. The FIRST time, the user reads the 6-digit code off the phone and types
//!    it in; FluxBridge runs `adb pair` then `adb connect`.
//! 4. The connected device is remembered ([`crate::devices`]) so the next
//!    launch auto-reconnects silently.
//!
//! The 6-digit code is only visible on the phone, so the first pairing is
//! inherently not zero-touch; this module does not pretend otherwise.

use std::sync::mpsc::Sender;

use crate::adb::{self, AdbCommand, DeviceSelector, MdnsService};
use crate::devices::{self, KnownDevices, PairedDevice};
use crate::pipeline::Transport;

/// A message a wireless worker thread sends back to the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WirelessEvent {
    /// A short status / progress line for the panel (adb provisioning, scan
    /// progress, pairing/connect results).
    Status(String),
    /// An error line for the panel (styled red). Non-terminal: the user can
    /// retry.
    Error(String),
    /// The refreshed list of discovered devices (label + stable id) to show.
    Devices(Vec<DiscoveredDeviceInfo>),
    /// A device was connected successfully; the payload is the Wi-Fi transport
    /// the mirroring pipeline should start against.
    Connected(Transport),
}

/// A discovered device flattened for the UI: a human-readable label plus the
/// stable id the UI passes back to connect/pair actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDeviceInfo {
    /// Display label, for example `adb-ABCDEF0123 (192.168.1.42:37000)`.
    pub label: String,
    /// Stable identifier: the mDNS instance name when known, else `host:port`.
    pub id: String,
}

/// Build the UI-facing device list from parsed mDNS services.
///
/// Only the CONNECT endpoints (`_adb-tls-connect._tcp`) are shown as connectable
/// rows, because a device only advertises a connect endpoint once it is paired
/// (or ready to be connected); the pairing endpoint is used transiently by the
/// one-time pair step. The id is the mDNS instance name (stable across IP
/// changes) so an already-paired row can be matched back to a remembered
/// device.
pub fn discovered_device_list(services: &[MdnsService]) -> Vec<DiscoveredDeviceInfo> {
    services
        .iter()
        .filter(|s| s.is_connect())
        .map(|s| DiscoveredDeviceInfo {
            label: format!("{} ({})", s.instance_name, s.address()),
            id: s.instance_name.clone(),
        })
        .collect()
}

/// Pick the pairing endpoint (`_adb-tls-pairing._tcp`) from discovered services.
///
/// Returns the first pairing service, which is what the one-time `adb pair`
/// step targets. `None` means the phone is not currently showing the "Pair
/// device with pairing code" screen (its pairing endpoint is only advertised
/// while that screen is open).
pub fn pairing_endpoint(services: &[MdnsService]) -> Option<&MdnsService> {
    services.iter().find(|s| s.is_pairing())
}

/// Pick the connect endpoint for a given selected id (mDNS instance name) from
/// discovered services, falling back to the first connect endpoint when the id
/// is empty or not matched.
pub fn connect_endpoint<'a>(
    services: &'a [MdnsService],
    selected_id: &str,
) -> Option<&'a MdnsService> {
    if !selected_id.is_empty() {
        if let Some(s) = services
            .iter()
            .find(|s| s.is_connect() && s.instance_name == selected_id)
        {
            return Some(s);
        }
    }
    services.iter().find(|s| s.is_connect())
}

/// Validate a user-entered pairing code: Android shows a 6-digit numeric code.
///
/// Trims surrounding whitespace and accepts exactly six ASCII digits. Returns
/// the cleaned code on success, or an error message suitable for the status
/// line. Keeping this pure means the UI never spawns a worker for an obviously
/// invalid code.
pub fn validate_pairing_code(raw: &str) -> Result<String, String> {
    let code = raw.trim();
    if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) {
        Ok(code.to_string())
    } else {
        Err("Enter the 6-digit pairing code shown on the phone.".to_string())
    }
}

/// Build the [`PairedDevice`] record to remember after a successful connect.
///
/// The connect service carries the stable mDNS instance name and the live
/// host/connect-port; this record is what [`crate::devices`] persists so a
/// returning user auto-reconnects.
pub fn paired_record(connect: &MdnsService) -> PairedDevice {
    let mut device = PairedDevice::new(connect.host.clone(), connect.port);
    device.instance_name = Some(connect.instance_name.clone());
    device.display_name = Some(connect.instance_name.clone());
    device.last_connected = Some(now_epoch_secs());
    device
}

/// Current Unix epoch seconds (0 if the clock is before the epoch).
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve an adb command bound to the default device, provisioning adb when it
/// is absent, and report provisioning progress onto `events`.
///
/// Returns `None` after sending an [`WirelessEvent::Error`] when adb cannot be
/// located or provisioned (offline), so the caller can abort the worker cleanly.
fn resolve_adb(events: &Sender<WirelessEvent>) -> Option<AdbCommand> {
    let data_dir = match crate::provision::data_dir() {
        Ok(dir) => dir,
        Err(_) => {
            let _ = events.send(WirelessEvent::Error(
                "Could not resolve the FluxBridge data directory.".to_string(),
            ));
            return None;
        }
    };

    // Prefer an installed/bundled adb; otherwise download Google's
    // platform-tools once, surfacing progress on the status line.
    if let Ok(path) = adb::locate_or_provisioned(&data_dir) {
        return Some(AdbCommand::with_path(path, DeviceSelector::UsbDefault));
    }

    let _ = events.send(WirelessEvent::Status(
        "Downloading Android platform-tools...".to_string(),
    ));
    let events_cb = events.clone();
    let mut progress = move |p: crate::provision::ProvisionProgress| match p {
        crate::provision::ProvisionProgress::Downloading(pct) => {
            let _ = events_cb.send(WirelessEvent::Status(format!(
                "Downloading Android platform-tools... {pct}%"
            )));
        }
        crate::provision::ProvisionProgress::Extracting => {
            let _ = events_cb.send(WirelessEvent::Status(
                "Extracting Android platform-tools...".to_string(),
            ));
        }
        crate::provision::ProvisionProgress::Done(_) => {}
    };
    let transport = fluxbridge_updater::UreqTransport::default();
    match crate::provision::ensure_adb(&data_dir, &transport, &mut progress) {
        // Bind the command to the path ensure_adb RETURNED so the just-installed
        // adb is used immediately, in THIS same worker invocation, with no app
        // restart. We deliberately do not re-run locate_adb here (which could
        // still miss the fresh copy on some hosts); the returned path is the
        // authoritative provisioned adb. Emit a clear status so the user sees
        // the install finished and the action is continuing without a restart.
        Ok(path) => {
            let _ = events.send(WirelessEvent::Status(format!(
                "Android platform-tools installed at {}. Continuing...",
                path.display()
            )));
            Some(AdbCommand::with_path(path, DeviceSelector::UsbDefault))
        }
        Err(_) => {
            let _ = events.send(WirelessEvent::Error(
                "adb is not installed and could not be downloaded. Check your internet \
                 connection, or install Android platform-tools and set ANDROID_HOME."
                    .to_string(),
            ));
            None
        }
    }
}

/// Scan the LAN for wireless-debugging devices and report them on `events`.
///
/// Provisions adb if needed, runs `adb mdns services`, parses the output, and
/// sends a [`WirelessEvent::Devices`] list (plus status lines). This is the
/// side-effecting worker body the UI runs on a background thread; the parsing
/// and list-building it relies on are pure and unit-tested.
pub fn scan(events: &Sender<WirelessEvent>) {
    let Some(cmd) = resolve_adb(events) else {
        return;
    };
    let _ = events.send(WirelessEvent::Status(
        "Searching for wireless devices...".to_string(),
    ));
    match cmd.mdns_services() {
        Ok(services) => {
            let list = discovered_device_list(&services);
            let count = list.len();
            let _ = events.send(WirelessEvent::Devices(list));
            let msg = if count == 0 {
                "No devices found. On the phone, open Wireless debugging (and 'Pair \
                 device with pairing code' for a first pairing), then Scan again."
                    .to_string()
            } else {
                format!("Found {count} device(s).")
            };
            let _ = events.send(WirelessEvent::Status(msg));
        }
        Err(e) => {
            let _ = events.send(WirelessEvent::Error(format!("mDNS discovery failed: {e}")));
        }
    }
}

/// Pair a NEW device with the 6-digit code and then connect it.
///
/// Validates the code, provisions adb, runs `adb mdns services` to find the
/// (dynamic) pairing and connect endpoints, runs `adb pair` then `adb connect`,
/// persists the device for auto-reconnect, and finally reports a
/// [`WirelessEvent::Connected`] carrying the Wi-Fi [`Transport`] for the
/// mirroring pipeline. Every failure is a non-terminal [`WirelessEvent::Error`]
/// so the user can retry.
pub fn pair_and_connect(events: &Sender<WirelessEvent>, raw_code: &str, selected_id: &str) {
    let code = match validate_pairing_code(raw_code) {
        Ok(c) => c,
        Err(msg) => {
            let _ = events.send(WirelessEvent::Error(msg));
            return;
        }
    };
    let Some(cmd) = resolve_adb(events) else {
        return;
    };

    let _ = events.send(WirelessEvent::Status(
        "Looking up the phone's pairing endpoint...".to_string(),
    ));
    let services = match cmd.mdns_services() {
        Ok(s) => s,
        Err(e) => {
            let _ = events.send(WirelessEvent::Error(format!("mDNS discovery failed: {e}")));
            return;
        }
    };

    let Some(pairing) = pairing_endpoint(&services) else {
        let _ = events.send(WirelessEvent::Error(
            "No pairing endpoint found. On the phone, open 'Pair device with pairing \
             code' so FluxBridge can see it, then try again."
                .to_string(),
        ));
        return;
    };
    let pairing = pairing.clone();

    let _ = events.send(WirelessEvent::Status(format!(
        "Pairing with {}...",
        pairing.address()
    )));
    if let Err(e) = cmd.pair(&pairing.host, pairing.port, &code) {
        let _ = events.send(WirelessEvent::Error(format!(
            "Pairing failed: {e}. Double-check the 6-digit code (it changes each time \
             you open the pairing screen)."
        )));
        return;
    }

    // After pairing, the device advertises a connect endpoint. It may not be in
    // the services list we fetched before pairing, so re-query to pick it up.
    let services = cmd.mdns_services().unwrap_or(services);
    let connect = match connect_endpoint(&services, selected_id) {
        Some(c) => c.clone(),
        None => {
            // Fall back to the pairing host; adb often reaches the device at the
            // same host once paired, but without a connect port we cannot
            // proceed, so report honestly.
            let _ = events.send(WirelessEvent::Error(
                "Paired, but no connect endpoint was advertised yet. Click Scan, then \
                 Connect the device from the list."
                    .to_string(),
            ));
            return;
        }
    };

    finish_connect(events, &cmd, &connect);
}

/// Connect an ALREADY-paired device (selected from the discovered list) and
/// start mirroring; no pairing code needed.
pub fn connect_device(events: &Sender<WirelessEvent>, selected_id: &str) {
    let Some(cmd) = resolve_adb(events) else {
        return;
    };
    let services = match cmd.mdns_services() {
        Ok(s) => s,
        Err(e) => {
            let _ = events.send(WirelessEvent::Error(format!("mDNS discovery failed: {e}")));
            return;
        }
    };
    let connect = match connect_endpoint(&services, selected_id) {
        Some(c) => c.clone(),
        None => {
            let _ = events.send(WirelessEvent::Error(
                "That device is no longer advertised. Click Scan and try again.".to_string(),
            ));
            return;
        }
    };
    finish_connect(events, &cmd, &connect);
}

/// Run `adb connect` against a discovered connect endpoint, persist the device,
/// and emit [`WirelessEvent::Connected`] on success.
fn finish_connect(events: &Sender<WirelessEvent>, cmd: &AdbCommand, connect: &MdnsService) {
    let _ = events.send(WirelessEvent::Status(format!(
        "Connecting to {}...",
        connect.address()
    )));
    match cmd.connect_discovered(&connect.host, connect.port) {
        Ok(conn) => {
            remember_device(connect);
            let _ = events.send(WirelessEvent::Status(format!(
                "Connected to {}. Starting mirroring...",
                conn.serial()
            )));
            let _ = events.send(WirelessEvent::Connected(Transport::from_wireless(&conn)));
        }
        Err(e) => {
            let _ = events.send(WirelessEvent::Error(format!("Connect failed: {e}")));
        }
    }
}

/// Persist the connected device so a future launch auto-reconnects silently.
///
/// Best-effort: a failure to write the file is not fatal to the current session
/// (the user is already connected), so it is ignored.
fn remember_device(connect: &MdnsService) {
    let Ok(data_dir) = crate::provision::data_dir() else {
        return;
    };
    let path = devices::devices_path(&data_dir);
    let mut known = KnownDevices::load(&path);
    known.upsert(paired_record(connect));
    let _ = known.save(&path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::MdnsService;

    fn svc(instance: &str, ty: &str, host: &str, port: u16) -> MdnsService {
        MdnsService {
            instance_name: instance.to_string(),
            service_type: ty.to_string(),
            host: host.to_string(),
            port,
        }
    }

    fn sample_services() -> Vec<MdnsService> {
        vec![
            svc(
                "adb-AAAA-1",
                adb::MDNS_CONNECT_TYPE,
                "192.168.1.42",
                37000,
            ),
            svc(
                "adb-AAAA-1",
                adb::MDNS_PAIRING_TYPE,
                "192.168.1.42",
                41355,
            ),
            svc(
                "adb-BBBB-2",
                adb::MDNS_CONNECT_TYPE,
                "192.168.1.99",
                44000,
            ),
        ]
    }

    #[test]
    fn device_list_shows_only_connect_endpoints() {
        let list = discovered_device_list(&sample_services());
        assert_eq!(list.len(), 2, "only connect endpoints are listed");
        assert_eq!(list[0].id, "adb-AAAA-1");
        assert_eq!(list[0].label, "adb-AAAA-1 (192.168.1.42:37000)");
        assert_eq!(list[1].id, "adb-BBBB-2");
    }

    #[test]
    fn pairing_endpoint_is_the_pairing_service() {
        let services = sample_services();
        let pairing = pairing_endpoint(&services).expect("a pairing endpoint");
        assert!(pairing.is_pairing());
        assert_eq!(pairing.port, 41355);
    }

    #[test]
    fn connect_endpoint_matches_selected_id_then_falls_back() {
        let services = sample_services();
        // Exact id match.
        let c = connect_endpoint(&services, "adb-BBBB-2").expect("matched connect");
        assert_eq!(c.port, 44000);
        // Empty id falls back to the first connect endpoint.
        let first = connect_endpoint(&services, "").expect("fallback connect");
        assert_eq!(first.port, 37000);
        // Unknown id also falls back to the first connect endpoint.
        let fallback = connect_endpoint(&services, "adb-UNKNOWN").expect("fallback connect");
        assert_eq!(fallback.port, 37000);
    }

    #[test]
    fn validate_pairing_code_accepts_six_digits_and_trims() {
        assert_eq!(validate_pairing_code(" 123456 ").unwrap(), "123456");
        assert!(validate_pairing_code("12345").is_err());
        assert!(validate_pairing_code("1234567").is_err());
        assert!(validate_pairing_code("12a456").is_err());
        assert!(validate_pairing_code("").is_err());
    }

    #[test]
    fn paired_record_carries_identity_and_endpoint() {
        let connect = svc("adb-AAAA-1", adb::MDNS_CONNECT_TYPE, "192.168.1.42", 37000);
        let record = paired_record(&connect);
        assert_eq!(record.serial, "192.168.1.42:37000");
        assert_eq!(record.host, "192.168.1.42");
        assert_eq!(record.connect_port, 37000);
        assert_eq!(record.instance_name.as_deref(), Some("adb-AAAA-1"));
        assert_eq!(record.display_name.as_deref(), Some("adb-AAAA-1"));
    }
}
