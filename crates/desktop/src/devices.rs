//! Persistence of remembered wireless (Wi-Fi) paired devices.
//!
//! Android 11+ wireless debugging requires a ONE-TIME pairing with the 6-digit
//! code the phone shows (see [`crate::adb`]). After that first pairing, a
//! device can be reached again with a plain `adb connect <host>:<connect-port>`
//! with no further user interaction. To make a returning user one-click (or
//! fully automatic), FluxBridge remembers each paired device in a small JSON
//! file in the per-user app data dir ([`crate::provision::data_dir`]) and, on
//! launch, tries to auto-reconnect to the most recently used one.
//!
//! All of this is pure, testable logic:
//!
//! * [`KnownDevices::load`] never panics: a missing OR corrupt file yields an
//!   empty set so a hand-edited or partially written file can never crash the
//!   app.
//! * [`KnownDevices::save`] writes pretty JSON (creating the parent dir).
//! * [`KnownDevices::upsert`] inserts or updates by the device serial and keeps
//!   the list ordered most-recently-used first, so [`KnownDevices::most_recent`]
//!   is the natural auto-reconnect target.
//!
//! The IP/port a phone advertises over mDNS can change between sessions (DHCP,
//! a dynamic adb port), so the stored `host`/`connect_port` is a best-effort
//! hint for a fast auto-reconnect; when it fails, the app falls back to mDNS
//! rediscovery (and, for a brand new device, the one-time pairing flow).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The fixed file name the known-devices list is stored under inside the app
/// data dir.
pub const DEVICES_FILE: &str = "paired-devices.json";

/// One remembered device the user has paired over Wi-Fi.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    /// The adb serial the device is reached by, i.e. `host:connect_port` at the
    /// time of pairing. Stored so a stale entry can be recognized/updated and
    /// used as the stable identity key for [`KnownDevices::upsert`].
    pub serial: String,
    /// The mDNS instance name, for example `adb-ABCDEF0123-AbCdEf`, when known.
    /// This is the stable per-device identity across IP changes; `None` for a
    /// device added without an mDNS record.
    #[serde(default)]
    pub instance_name: Option<String>,
    /// Last known host (IP address) of the device's connect endpoint.
    pub host: String,
    /// Last known (dynamic) connect port of the device.
    pub connect_port: u16,
    /// A friendly name for the UI; defaults to the serial when unset.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Unix epoch seconds of the last successful connection, used to order the
    /// list most-recently-used first. `None` if never recorded.
    #[serde(default)]
    pub last_connected: Option<u64>,
}

impl PairedDevice {
    /// Build a device record from a discovered/connected host and connect port.
    pub fn new(host: impl Into<String>, connect_port: u16) -> Self {
        let host = host.into();
        let serial = format!("{host}:{connect_port}");
        PairedDevice {
            serial,
            instance_name: None,
            host,
            connect_port,
            display_name: None,
            last_connected: None,
        }
    }

    /// The display name to show in the UI: the explicit `display_name` when set,
    /// otherwise the serial.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.serial)
    }
}

/// The remembered set of paired devices, ordered most-recently-used first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownDevices {
    /// The devices, most-recently-used first (index 0 is [`most_recent`]).
    ///
    /// [`most_recent`]: KnownDevices::most_recent
    #[serde(default)]
    devices: Vec<PairedDevice>,
}

impl KnownDevices {
    /// An empty set.
    pub fn new() -> Self {
        KnownDevices::default()
    }

    /// Load the known-devices list from `path`.
    ///
    /// Returns an EMPTY set (never an error, never a panic) when the file does
    /// not exist, cannot be read, or contains invalid/corrupt JSON, so a
    /// first-run or a hand-edited file can never crash the app. A returning
    /// user with a valid file gets their remembered devices back.
    pub fn load(path: &Path) -> KnownDevices {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return KnownDevices::default(),
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Serialize the set to `path` as pretty JSON, creating the parent dir.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Number of remembered devices.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Whether no devices are remembered.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Iterate the devices, most-recently-used first.
    pub fn iter(&self) -> std::slice::Iter<'_, PairedDevice> {
        self.devices.iter()
    }

    /// The most-recently-used device, the natural auto-reconnect target.
    pub fn most_recent(&self) -> Option<&PairedDevice> {
        self.devices.first()
    }

    /// Insert `device`, or update an existing entry with the same identity, and
    /// move it to the front (most-recently-used).
    ///
    /// Identity is matched by `instance_name` when both carry one (so a device
    /// whose IP/port changed is updated, not duplicated), otherwise by
    /// `serial`. The upserted device ends up at index 0 so [`most_recent`]
    /// returns it.
    ///
    /// [`most_recent`]: KnownDevices::most_recent
    pub fn upsert(&mut self, device: PairedDevice) {
        let pos = self.devices.iter().position(|d| same_identity(d, &device));
        if let Some(i) = pos {
            self.devices.remove(i);
        }
        self.devices.insert(0, device);
    }
}

/// Whether two device records refer to the same physical device: by mDNS
/// instance name when both have one, otherwise by serial.
fn same_identity(a: &PairedDevice, b: &PairedDevice) -> bool {
    match (&a.instance_name, &b.instance_name) {
        (Some(x), Some(y)) => x == y,
        _ => a.serial == b.serial,
    }
}

/// The path the known-devices JSON lives at inside a given data dir.
pub fn devices_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(DEVICES_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "fluxbridge-devices-{}-{}-{}.json",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn load_missing_file_yields_empty_set() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let known = KnownDevices::load(&path);
        assert!(known.is_empty());
        assert_eq!(known.len(), 0);
        assert!(known.most_recent().is_none());
    }

    #[test]
    fn load_corrupt_file_yields_empty_set_not_panic() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"this is not valid json {{{").unwrap();
        let known = KnownDevices::load(&path);
        assert!(known.is_empty(), "corrupt JSON must load as empty, not panic");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upsert_save_load_round_trips_two_devices() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let mut known = KnownDevices::new();
        let mut a = PairedDevice::new("192.168.1.42", 37000);
        a.instance_name = Some("adb-AAAA-1".into());
        a.display_name = Some("Pixel".into());
        a.last_connected = Some(1000);
        let mut b = PairedDevice::new("192.168.1.99", 44000);
        b.instance_name = Some("adb-BBBB-2".into());
        b.last_connected = Some(2000);

        known.upsert(a.clone());
        known.upsert(b.clone()); // b is inserted last so it is most-recent

        assert_eq!(known.len(), 2);
        assert_eq!(known.most_recent(), Some(&b));

        known.save(&path).expect("save round-trip file");
        let loaded = KnownDevices::load(&path);
        assert_eq!(loaded, known, "loaded set equals saved set");
        assert_eq!(loaded.most_recent(), Some(&b));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upsert_updates_existing_identity_and_moves_to_front() {
        let mut known = KnownDevices::new();
        let mut a = PairedDevice::new("192.168.1.42", 37000);
        a.instance_name = Some("adb-SAME".into());
        let b = PairedDevice::new("192.168.1.99", 44000);
        known.upsert(a.clone());
        known.upsert(b.clone());
        assert_eq!(known.len(), 2);

        // Same device (same instance_name) but a new IP/port: update in place,
        // do not duplicate, and move to the front.
        let mut a_moved = PairedDevice::new("192.168.1.50", 50000);
        a_moved.instance_name = Some("adb-SAME".into());
        known.upsert(a_moved.clone());

        assert_eq!(known.len(), 2, "same identity must not duplicate");
        assert_eq!(known.most_recent(), Some(&a_moved));
        assert_eq!(known.most_recent().unwrap().host, "192.168.1.50");
        assert_eq!(known.most_recent().unwrap().connect_port, 50000);
    }

    #[test]
    fn label_falls_back_to_serial() {
        let d = PairedDevice::new("10.0.0.1", 5555);
        assert_eq!(d.label(), "10.0.0.1:5555");
        let mut named = d.clone();
        named.display_name = Some("Office phone".into());
        assert_eq!(named.label(), "Office phone");
    }
}
