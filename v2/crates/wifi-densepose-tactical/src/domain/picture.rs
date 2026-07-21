//! The aggregated live picture handed to the operator.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::contact::PersonContact;
use super::structure::{Room, RoomBounds, RoomId};

/// Per-room roll-up of what the sensors currently see.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomOccupancy {
    /// Room id.
    pub room_id: RoomId,
    /// Room label.
    pub room_name: String,
    /// Storey.
    pub floor: i32,
    /// Room footprint (echoed so a UI can render without the full structure).
    pub bounds: RoomBounds,
    /// Number of tracked contacts currently in the room.
    pub contact_count: usize,
    /// Best occupancy estimate for the room (>= `contact_count`). WiFi counting
    /// is coarse — treat as a lower bound.
    pub occupancy_estimate: u32,
    /// Whether any contact in the room is moving.
    pub any_moving: bool,
    /// Whether the room has sensor geometry for a point fix.
    pub localizable: bool,
    /// Latest mmWave (ESP32-C6 / MR60BHA2) reading for this room, if a mmWave
    /// node covers it and reported recently. Independent physics from WiFi CSI.
    #[serde(default)]
    pub mmwave: Option<MmwaveCorroboration>,
    /// `true` when BOTH the CSI mesh (a contact here) AND a fresh mmWave reading
    /// agree that the room is occupied — the strongest presence evidence.
    #[serde(default)]
    pub corroborated: bool,
}

/// An independent mmWave vital-sign reading for a room, used to corroborate (or
/// contradict) the CSI-derived contact. From an ESP32-C6 + Seeed MR60BHA2 node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MmwaveCorroboration {
    /// mmWave says a person is present.
    pub presence: bool,
    /// mmWave breathing rate (breaths/min), if resolved (> 0).
    pub breathing_bpm: Option<f32>,
    /// mmWave heart rate (beats/min), if resolved (> 0).
    pub heart_rate_bpm: Option<f32>,
    /// Distance to nearest target (cm), if reported.
    pub distance_cm: Option<f32>,
    /// Target count from the radar.
    pub targets: u8,
    /// mmWave signal-quality score 0–100.
    pub confidence: u8,
    /// Seconds since this reading arrived.
    pub age_secs: i64,
}

/// Health of a single sensor node, as of the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorSummary {
    /// Node id (matches a `SensorPlacement::id` and the `id` in RSSI readings).
    pub id: String,
    /// Room the node is assigned to (or last reported from).
    pub room_id: Option<RoomId>,
    /// Room label, denormalized for display.
    pub room_name: Option<String>,
    /// `true` if the node is part of the loaded floor plan; `false` for an
    /// unexpected node that reported but is not in the structure.
    pub configured: bool,
    /// `true` if the node reported within the freshness window.
    pub reporting: bool,
    /// Seconds since the node was last heard from (`None` = never seen).
    pub age_secs: Option<i64>,
    /// Most recent RSSI reported by the node (dBm).
    pub last_rssi: Option<f64>,
}

/// A Bluetooth device detected by the phone's own radio.
///
/// **This is a device, not a person.** A single phone measures signal strength
/// (hence a rough distance) but no bearing, and only sees devices that are
/// actively transmitting BLE. It cannot detect a person who is not carrying a
/// discoverable device, and it never implies a floor-plan position — which is
/// why these are shown in a separate list, never as contacts on the map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BleDevice {
    /// Opaque device id (the advertised address — often randomized by the OS).
    pub id: String,
    /// Advertised name, if any.
    pub name: Option<String>,
    /// Most recent RSSI (dBm).
    pub rssi: f64,
    /// Very rough distance estimate from RSSI (metres) — order-of-magnitude only.
    pub distance_est_m: f64,
    /// Seconds since last seen.
    pub age_secs: i64,
}

/// A snapshot of the whole structure at a moment in time.
///
/// This is the payload streamed to the tactical dashboard and returned by
/// `GET /api/picture`. It is advisory only — see the crate-level safety notes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TacticalPicture {
    /// Structure label.
    pub structure_name: String,
    /// When this snapshot was generated.
    pub generated_at: DateTime<Utc>,
    /// Per-room roll-up, in structure room order.
    pub rooms: Vec<RoomOccupancy>,
    /// Every tracked contact across the structure.
    pub contacts: Vec<PersonContact>,
    /// Sum of room occupancy estimates — a coarse "how many people inside".
    pub total_occupancy_estimate: u32,
    /// Number of rooms with at least one contact.
    pub occupied_rooms: usize,
    /// Per-node sensor health.
    pub sensors: Vec<SensorSummary>,
    /// How many nodes are currently reporting.
    pub sensors_reporting: usize,
    /// How many nodes are configured in the floor plan.
    pub sensors_total: usize,
    /// Bluetooth devices detected by the phone's own radio (auxiliary layer —
    /// devices, NOT people; no floor-plan position). Attached by the API from
    /// the BLE tracker; empty from the engine alone.
    #[serde(default)]
    pub ble_devices: Vec<BleDevice>,
    /// A standing reminder rendered by clients. Kept in the payload so it can
    /// never be dropped by a thin viewer.
    pub advisory: String,
}

/// The advisory carried on every picture.
pub const STANDING_ADVISORY: &str =
    "Decision-support only. Contacts are anonymous human presence — NOT identified \
     as hostage or suspect, NOT confirmed armed. Absence of a contact does not mean \
     a room is empty. Corroborate before acting.";

impl TacticalPicture {
    /// Assemble a picture from the structure's rooms and the current contacts.
    #[allow(clippy::too_many_arguments)]
    pub fn assemble(
        structure_name: &str,
        rooms: &[Room],
        contacts: Vec<PersonContact>,
        occupancy_by_room: &[(RoomId, u32)],
        sensors: Vec<SensorSummary>,
        mmwave_by_room: &[(RoomId, MmwaveCorroboration)],
    ) -> Self {
        let room_rollup: Vec<RoomOccupancy> = rooms
            .iter()
            .map(|room| {
                let in_room: Vec<&PersonContact> =
                    contacts.iter().filter(|c| c.room_id == room.id).collect();
                let occupancy_estimate = occupancy_by_room
                    .iter()
                    .find(|(id, _)| *id == room.id)
                    .map(|(_, n)| *n)
                    .unwrap_or(0)
                    .max(in_room.len() as u32);
                let mmwave = mmwave_by_room
                    .iter()
                    .find(|(id, _)| *id == room.id)
                    .map(|(_, m)| m.clone());
                let corroborated =
                    !in_room.is_empty() && mmwave.as_ref().map(|m| m.presence).unwrap_or(false);
                RoomOccupancy {
                    room_id: room.id,
                    room_name: room.name.clone(),
                    floor: room.floor,
                    bounds: room.bounds,
                    contact_count: in_room.len(),
                    occupancy_estimate,
                    any_moving: in_room
                        .iter()
                        .any(|c| c.motion == super::contact::Motion::Moving),
                    localizable: room.localizable(),
                    mmwave,
                    corroborated,
                }
            })
            .collect();

        let total_occupancy_estimate = room_rollup.iter().map(|r| r.occupancy_estimate).sum();
        let occupied_rooms = room_rollup.iter().filter(|r| r.contact_count > 0).count();
        let sensors_reporting = sensors.iter().filter(|s| s.reporting).count();
        let sensors_total = sensors.iter().filter(|s| s.configured).count();

        Self {
            structure_name: structure_name.to_string(),
            generated_at: Utc::now(),
            rooms: room_rollup,
            contacts,
            total_occupancy_estimate,
            occupied_rooms,
            sensors,
            sensors_reporting,
            sensors_total,
            ble_devices: Vec::new(),
            advisory: STANDING_ADVISORY.to_string(),
        }
    }
}
