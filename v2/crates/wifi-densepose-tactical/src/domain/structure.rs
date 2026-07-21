//! The structure being cleared: a building modelled as rooms with sensor placement.

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wifi_densepose_mat::domain::{ScanZone, SensorPosition, ZoneBounds};

/// Stable identifier for a [`Structure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StructureId(Uuid);

impl StructureId {
    /// Generate a fresh random id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for StructureId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for StructureId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Stable identifier for a [`Room`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoomId(Uuid);

impl RoomId {
    /// Generate a fresh random id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap an existing UUID (used when a caller supplies a stable id).
    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }
}

impl Default for RoomId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RoomId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A single room (or bounded area) inside a [`Structure`].
///
/// Coordinates are metres in a floor-plan frame chosen by the operator — the
/// origin and axes are whatever the sketch/blueprint used. `floor` lets a
/// multi-storey building share one coordinate frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    /// Stable id.
    pub id: RoomId,
    /// Human label, e.g. "Master Bedroom", "SW stairwell".
    pub name: String,
    /// Storey number (ground = 0, basement negative).
    pub floor: i32,
    /// Rectangular footprint in the floor-plan frame.
    pub bounds: RoomBounds,
    /// WiFi sensor nodes covering this room. Three or more with live RSSI are
    /// needed for a point fix; fewer yields room-level presence only.
    #[serde(default)]
    pub sensors: Vec<SensorPlacement>,
}

/// Axis-aligned rectangular footprint of a room, in metres.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RoomBounds {
    /// Minimum X (metres).
    pub min_x: f64,
    /// Minimum Y (metres).
    pub min_y: f64,
    /// Maximum X (metres).
    pub max_x: f64,
    /// Maximum Y (metres).
    pub max_y: f64,
}

impl RoomBounds {
    /// Construct from a corner and a size.
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// Centroid of the room.
    pub fn center(&self) -> (f64, f64) {
        ((self.min_x + self.max_x) / 2.0, (self.min_y + self.max_y) / 2.0)
    }

    /// Radius of the enclosing circle around the centroid (metres). Used as the
    /// uncertainty radius when only room-level presence is known.
    pub fn enclosing_radius(&self) -> f64 {
        let (cx, cy) = self.center();
        let dx = self.max_x - cx;
        let dy = self.max_y - cy;
        (dx * dx + dy * dy).sqrt()
    }

    /// Whether a floor-plan point falls inside this room.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    fn to_zone_bounds(self) -> ZoneBounds {
        ZoneBounds::rectangle(self.min_x, self.min_y, self.max_x, self.max_y)
    }
}

/// A WiFi sensor node placed for coverage of a room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorPlacement {
    /// Node id (matches the `id` in incoming RSSI readings).
    pub id: String,
    /// X position (metres, floor-plan frame).
    pub x: f64,
    /// Y position (metres, floor-plan frame).
    pub y: f64,
    /// Height above the floor (metres).
    #[serde(default = "default_sensor_height")]
    pub z: f64,
}

fn default_sensor_height() -> f64 {
    1.2
}

impl Room {
    /// Create a room with a rectangular footprint and no sensors yet.
    pub fn new(name: impl Into<String>, floor: i32, bounds: RoomBounds) -> Self {
        Self {
            id: RoomId::new(),
            name: name.into(),
            floor,
            bounds,
            sensors: Vec::new(),
        }
    }

    /// Add a sensor node, returning `self` for chaining.
    pub fn with_sensor(mut self, id: impl Into<String>, x: f64, y: f64) -> Self {
        self.sensors.push(SensorPlacement {
            id: id.into(),
            x,
            y,
            z: default_sensor_height(),
        });
        self
    }

    /// Number of sensors placed for this room.
    pub fn sensor_count(&self) -> usize {
        self.sensors.len()
    }

    /// Whether the room can, in principle, be point-localized (>= 3 sensors).
    pub fn localizable(&self) -> bool {
        self.sensors.len() >= 3
    }

    /// Build a MAT [`ScanZone`] for this room, optionally injecting live RSSI
    /// (keyed by sensor id) so MAT's localizer can triangulate. Sensors without
    /// a live reading are passed with `last_rssi: None` — MAT never fabricates.
    pub fn to_scan_zone(&self, rssi_by_id: &[(String, f64)]) -> ScanZone {
        let mut zone = ScanZone::new(&self.name, self.bounds.to_zone_bounds());
        for s in &self.sensors {
            let last_rssi = rssi_by_id
                .iter()
                .find(|(id, _)| id == &s.id)
                .map(|(_, r)| *r);
            zone.add_sensor(SensorPosition {
                id: s.id.clone(),
                x: s.x,
                y: s.y,
                z: s.z,
                sensor_type: wifi_densepose_mat::domain::SensorType::Transceiver,
                is_operational: true,
                last_rssi,
            });
        }
        zone
    }
}

/// A building being cleared — the aggregate the operator loads before a callout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Structure {
    /// Stable id.
    pub id: StructureId,
    /// Label, e.g. an address or callsign.
    pub name: String,
    /// Rooms in floor-plan order.
    pub rooms: Vec<Room>,
}

impl Structure {
    /// Create an empty structure with a name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: StructureId::new(),
            name: name.into(),
            rooms: Vec::new(),
        }
    }

    /// Add a room, returning `self` for chaining.
    pub fn with_room(mut self, room: Room) -> Self {
        self.rooms.push(room);
        self
    }

    /// Look up a room by id.
    pub fn room(&self, id: RoomId) -> Option<&Room> {
        self.rooms.iter().find(|r| r.id == id)
    }

    /// Resolve a room by exact (case-insensitive) name, for name-keyed readings.
    pub fn room_by_name(&self, name: &str) -> Option<&Room> {
        self.rooms
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_center_and_radius() {
        let b = RoomBounds::new(0.0, 0.0, 4.0, 3.0);
        assert_eq!(b.center(), (2.0, 1.5));
        assert!((b.enclosing_radius() - 2.5).abs() < 1e-9);
        assert!(b.contains(1.0, 1.0));
        assert!(!b.contains(5.0, 1.0));
    }

    #[test]
    fn room_localizable_needs_three_sensors() {
        let room = Room::new("Den", 0, RoomBounds::new(0.0, 0.0, 4.0, 4.0))
            .with_sensor("a", 0.0, 0.0)
            .with_sensor("b", 4.0, 0.0);
        assert!(!room.localizable());
        let room = room.with_sensor("c", 2.0, 4.0);
        assert!(room.localizable());
    }

    #[test]
    fn to_scan_zone_injects_matching_rssi_only() {
        let room = Room::new("Den", 0, RoomBounds::new(0.0, 0.0, 4.0, 4.0))
            .with_sensor("a", 0.0, 0.0)
            .with_sensor("b", 4.0, 0.0)
            .with_sensor("c", 2.0, 4.0);
        let zone = room.to_scan_zone(&[("a".into(), -55.0), ("c".into(), -60.0)]);
        let sensors = zone.sensor_positions();
        assert_eq!(sensors.iter().filter(|s| s.last_rssi.is_some()).count(), 2);
        assert!(sensors.iter().find(|s| s.id == "b").unwrap().last_rssi.is_none());
    }

    #[test]
    fn structure_room_lookup() {
        let room = Room::new("Kitchen", 0, RoomBounds::new(0.0, 0.0, 3.0, 3.0));
        let id = room.id;
        let s = Structure::new("123 Main").with_room(room);
        assert!(s.room(id).is_some());
        assert!(s.room_by_name("kitchen").is_some());
        assert!(s.room_by_name("garage").is_none());
    }
}
