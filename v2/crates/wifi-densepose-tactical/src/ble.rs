//! Bluetooth device-presence tracker (auxiliary phone-native layer).
//!
//! The phone's own BLE radio can see *other devices that are advertising* and
//! estimate a rough distance from RSSI — but nothing about bearing, and nothing
//! about people who aren't carrying a discoverable device. This tracker keeps a
//! short-lived list of such devices for the "devices (not people)" overlay. It
//! deliberately produces **no floor-plan position**.
//!
//! Requires the `api` feature.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::domain::picture::BleDevice;

/// Max BLE devices tracked at once (bounds memory against a spoofed-address flood).
pub const MAX_DEVICES: usize = 512;
/// Max device-id length kept.
const MAX_ID_LEN: usize = 64;
/// Max advertised-name length kept.
const MAX_NAME_LEN: usize = 64;

/// One BLE observation reported by the phone scanner.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BleObservation {
    /// Device address / id (usually an OS-randomized MAC).
    pub id: String,
    /// Advertised name, if present.
    #[serde(default)]
    pub name: Option<String>,
    /// Measured RSSI (dBm).
    pub rssi: f64,
}

#[derive(Debug, Clone)]
struct DeviceState {
    name: Option<String>,
    rssi: f64,
    last_seen: DateTime<Utc>,
}

/// Tracks recently-seen BLE devices and turns them into ranked [`BleDevice`]s.
pub struct BleTracker {
    devices: HashMap<String, DeviceState>,
    /// Devices unseen for longer than this are dropped.
    stale_secs: i64,
    /// Log-distance path-loss exponent (2.0 free space, higher indoors).
    path_loss_n: f64,
    /// Reference RSSI at 1 m (dBm).
    tx_power_1m: f64,
}

impl Default for BleTracker {
    fn default() -> Self {
        Self {
            devices: HashMap::new(),
            stale_secs: 8,
            path_loss_n: 2.5,
            tx_power_1m: -59.0,
        }
    }
}

impl BleTracker {
    /// New tracker with default indoor parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a batch of observations (from one scan flush).
    ///
    /// Bounded against a hostile/spoofed `/api/ble` flood: ids and names are
    /// length-clamped, and the tracked set is capped at [`MAX_DEVICES`] by
    /// evicting the weakest-signal device (the closest devices are what matter).
    pub fn observe(&mut self, obs: &[BleObservation]) {
        let now = Utc::now();
        for o in obs {
            let id: String = o.id.chars().take(MAX_ID_LEN).collect();
            let name = o
                .name
                .as_ref()
                .map(|n| n.chars().take(MAX_NAME_LEN).collect::<String>());

            if let Some(entry) = self.devices.get_mut(&id) {
                entry.rssi = o.rssi;
                entry.last_seen = now;
                if name.is_some() {
                    entry.name = name;
                }
                continue;
            }

            // New device: enforce the cap by evicting the weakest tracked signal;
            // if the newcomer is weaker than everything tracked, drop it instead.
            if self.devices.len() >= MAX_DEVICES {
                if let Some((weak_id, weak_rssi)) = self
                    .devices
                    .iter()
                    .map(|(k, v)| (k.clone(), v.rssi))
                    .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                {
                    if o.rssi <= weak_rssi {
                        continue;
                    }
                    self.devices.remove(&weak_id);
                }
            }
            self.devices.insert(
                id,
                DeviceState {
                    name,
                    rssi: o.rssi,
                    last_seen: now,
                },
            );
        }
    }

    /// Drop devices not seen within the freshness window.
    pub fn prune(&mut self) {
        self.prune_at(Utc::now());
    }

    /// Prune against an explicit `now` (testable without wall-clock waits).
    pub fn prune_at(&mut self, now: DateTime<Utc>) {
        let stale = self.stale_secs;
        self.devices
            .retain(|_, d| (now - d.last_seen).num_seconds() <= stale);
    }

    /// Rough distance estimate from RSSI via the log-distance path-loss model.
    /// Order-of-magnitude only — clamped to a sane range.
    fn distance(&self, rssi: f64) -> f64 {
        let d = 10f64.powf((self.tx_power_1m - rssi) / (10.0 * self.path_loss_n));
        d.clamp(0.1, 100.0)
    }

    /// Current devices, closest (strongest RSSI) first.
    pub fn snapshot(&self) -> Vec<BleDevice> {
        self.snapshot_at(Utc::now())
    }

    /// Snapshot against an explicit `now`.
    pub fn snapshot_at(&self, now: DateTime<Utc>) -> Vec<BleDevice> {
        let mut out: Vec<BleDevice> = self
            .devices
            .iter()
            .map(|(id, d)| BleDevice {
                id: id.clone(),
                name: d.name.clone(),
                rssi: d.rssi,
                distance_est_m: self.distance(d.rssi),
                age_secs: (now - d.last_seen).num_seconds().max(0),
            })
            .collect();
        // Strongest signal (closest) first; ties broken by id for stability.
        out.sort_by(|a, b| {
            b.rssi
                .partial_cmp(&a.rssi)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// Number of tracked devices.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Whether any devices are tracked.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn obs(id: &str, name: Option<&str>, rssi: f64) -> BleObservation {
        BleObservation {
            id: id.to_string(),
            name: name.map(String::from),
            rssi,
        }
    }

    #[test]
    fn observe_and_snapshot_orders_by_proximity() {
        let mut t = BleTracker::new();
        t.observe(&[
            obs("aa", Some("Watch"), -80.0),
            obs("bb", None, -55.0),
            obs("cc", Some("Buds"), -67.0),
        ]);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 3);
        // Strongest RSSI (closest) first.
        assert_eq!(snap[0].id, "bb");
        assert_eq!(snap[2].id, "aa");
        // Closer device has a smaller distance estimate.
        assert!(snap[0].distance_est_m < snap[2].distance_est_m);
    }

    #[test]
    fn stronger_rssi_is_closer() {
        let t = BleTracker::new();
        assert!(t.distance(-50.0) < t.distance(-80.0));
        // At the 1m reference power, distance is ~1m.
        assert!((t.distance(-59.0) - 1.0).abs() < 0.2);
    }

    #[test]
    fn stale_devices_are_pruned() {
        let mut t = BleTracker::new();
        t.observe(&[obs("aa", None, -60.0)]);
        assert_eq!(t.len(), 1);
        t.prune_at(Utc::now() + Duration::seconds(3600));
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn device_map_is_capped_and_keeps_strongest() {
        let mut t = BleTracker::new();
        // Flood with more than the cap; rssi increases with index (stronger).
        for i in 0..(MAX_DEVICES + 200) {
            t.observe(&[obs(&format!("dev-{i}"), None, -120.0 + i as f64 * 0.01)]);
        }
        assert!(t.len() <= MAX_DEVICES, "device map must stay bounded");
        // The strongest (highest-index) device survived; the weakest did not.
        let snap = t.snapshot();
        assert!(snap.iter().any(|d| d.id == format!("dev-{}", MAX_DEVICES + 199)));
        assert!(!snap.iter().any(|d| d.id == "dev-0"));
    }

    #[test]
    fn long_id_and_name_are_truncated() {
        let mut t = BleTracker::new();
        let long = "x".repeat(500);
        t.observe(&[obs(&long, Some(&long), -50.0)]);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap[0].id.len() <= 64);
        assert!(snap[0].name.as_ref().unwrap().len() <= 64);
    }

    #[test]
    fn repeated_observation_updates_rssi_and_name() {
        let mut t = BleTracker::new();
        t.observe(&[obs("aa", None, -70.0)]);
        t.observe(&[obs("aa", Some("Phone"), -55.0)]);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].rssi, -55.0);
        assert_eq!(snap[0].name.as_deref(), Some("Phone"));
    }
}
