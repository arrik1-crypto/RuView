//! The tactical engine: ingest readings, localize, track contacts, emit pictures.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use wifi_densepose_mat::localization::LocalizationService;

use crate::domain::contact::{ContactId, PersonContact};
use crate::domain::picture::TacticalPicture;
use crate::domain::reading::RoomReading;
use crate::domain::structure::{Room, RoomId, Structure};
use crate::error::TacticalError;

/// Tunables for tracking behaviour.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Position smoothing factor in `[0, 1]`: fraction of the *new* fix blended
    /// into an existing contact each update (higher = more responsive, noisier).
    pub position_alpha: f64,
    /// A contact not refreshed within this many seconds is dropped by
    /// [`TacticalEngine::prune_stale`]. Keeps the picture from showing ghosts
    /// after a subject leaves or sensing stops.
    pub contact_ttl_secs: i64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            position_alpha: 0.5,
            contact_ttl_secs: 8,
        }
    }
}

/// Stateful tactical engine. Not `Sync`-shared internally — wrap in a lock for
/// concurrent use (the API does this).
pub struct TacticalEngine {
    structure: Structure,
    localizer: LocalizationService,
    /// One representative contact per occupied room. WiFi's aggregate per-room
    /// reading does not separate co-located people, so the room-level
    /// `occupancy` count below carries "how many" while this carries "where".
    contacts: HashMap<RoomId, PersonContact>,
    /// Latest occupancy estimate per room (>= 1 when occupied, 0 when cleared).
    occupancy: HashMap<RoomId, u32>,
    config: EngineConfig,
}

impl TacticalEngine {
    /// Create an engine for a structure.
    pub fn new(structure: Structure) -> Self {
        Self {
            structure,
            localizer: LocalizationService::new(),
            contacts: HashMap::new(),
            occupancy: HashMap::new(),
            config: EngineConfig::default(),
        }
    }

    /// Create with custom config.
    pub fn with_config(structure: Structure, config: EngineConfig) -> Self {
        Self {
            config,
            ..Self::new(structure)
        }
    }

    /// Replace the loaded structure, discarding all tracked contacts.
    pub fn set_structure(&mut self, structure: Structure) {
        self.structure = structure;
        self.contacts.clear();
        self.occupancy.clear();
    }

    /// The loaded structure.
    pub fn structure(&self) -> &Structure {
        &self.structure
    }

    /// Resolve a room by id or name (name is case-insensitive).
    pub fn resolve_room(&self, id: Option<RoomId>, name: Option<&str>) -> Option<RoomId> {
        if let Some(id) = id {
            if self.structure.room(id).is_some() {
                return Some(id);
            }
        }
        if let Some(name) = name {
            return self.structure.room_by_name(name).map(|r| r.id);
        }
        None
    }

    /// Ingest a resolved reading for a room, updating (or creating) its contact.
    ///
    /// Localization is attempted first via MAT: with >= 3 sensors carrying live
    /// RSSI it yields a real point fix; otherwise the contact is placed at the
    /// room centroid with the room's enclosing radius as uncertainty (room-level
    /// presence). Existing contacts are smoothed toward the new position.
    pub fn ingest(&mut self, reading: RoomReading) -> Result<(), TacticalError> {
        let room = self
            .structure
            .room(reading.room_id)
            .ok_or_else(|| TacticalError::UnknownRoom(reading.room_id.to_string()))?
            .clone();

        let now = Utc::now();
        self.occupancy.insert(room.id, reading.occupancy.max(1));

        let (x, y, point_fix, uncertainty) = self.localize(&room, &reading);
        let motion = PersonContact::classify_motion(&reading.vitals);
        let life_sign = PersonContact::classify_life_sign(&reading.vitals);
        let breathing_bpm = reading.vitals.breathing.as_ref().map(|b| b.rate_bpm);
        let confidence = reading.vitals.confidence.value();

        match self.contacts.get_mut(&room.id) {
            Some(existing) => {
                let a = self.config.position_alpha;
                existing.x = existing.x * (1.0 - a) + x * a;
                existing.y = existing.y * (1.0 - a) + y * a;
                existing.point_fix = point_fix;
                existing.uncertainty_radius_m = uncertainty;
                existing.motion = motion;
                existing.life_sign = life_sign;
                existing.breathing_bpm = breathing_bpm;
                existing.confidence = confidence;
                existing.last_updated = now;
            }
            None => {
                self.contacts.insert(
                    room.id,
                    PersonContact {
                        id: ContactId::new(),
                        room_id: room.id,
                        room_name: room.name.clone(),
                        x,
                        y,
                        point_fix,
                        uncertainty_radius_m: uncertainty,
                        motion,
                        life_sign,
                        breathing_bpm,
                        confidence,
                        first_seen: now,
                        last_updated: now,
                    },
                );
            }
        }
        Ok(())
    }

    /// Apply a raw [`ReadingInput`](crate::domain::reading::ReadingInput):
    /// resolve its room (by id or name), then ingest a detection or record an
    /// absence depending on whether presence was reported. This is the entry
    /// point the API uses so the web layer stays free of domain conversions.
    pub fn apply_input(
        &mut self,
        input: &crate::domain::reading::ReadingInput,
    ) -> Result<(), TacticalError> {
        let room_id = self
            .resolve_room(input.room_id, input.room_name.as_deref())
            .ok_or_else(|| {
                TacticalError::UnknownRoom(
                    input
                        .room_name
                        .clone()
                        .or_else(|| input.room_id.map(|id| id.to_string()))
                        .unwrap_or_else(|| "unspecified".to_string()),
                )
            })?;
        match input.to_vitals() {
            Some(vitals) => {
                let reading = RoomReading {
                    room_id,
                    vitals,
                    occupancy: input.effective_occupancy(),
                    sensor_rssi: input
                        .sensor_rssi
                        .iter()
                        .map(|s| (s.id.clone(), s.rssi))
                        .collect(),
                };
                self.ingest(reading)
            }
            None => self.record_absence(room_id),
        }
    }

    /// Record that a room was scanned and found clear: drop its contact and zero
    /// its occupancy. (Absence is not proof a room is empty — see crate docs.)
    pub fn record_absence(&mut self, room_id: RoomId) -> Result<(), TacticalError> {
        if self.structure.room(room_id).is_none() {
            return Err(TacticalError::UnknownRoom(room_id.to_string()));
        }
        self.contacts.remove(&room_id);
        self.occupancy.insert(room_id, 0);
        Ok(())
    }

    /// Drop contacts not refreshed within the configured TTL. Returns how many
    /// were removed.
    pub fn prune_stale(&mut self) -> usize {
        let cutoff = Utc::now() - Duration::seconds(self.config.contact_ttl_secs);
        self.prune_stale_before(cutoff)
    }

    /// TTL prune against an explicit cutoff (testable without wall-clock waits).
    pub fn prune_stale_before(&mut self, cutoff: DateTime<Utc>) -> usize {
        let before = self.contacts.len();
        let stale: Vec<RoomId> = self
            .contacts
            .iter()
            .filter(|(_, c)| c.last_updated < cutoff)
            .map(|(id, _)| *id)
            .collect();
        for id in &stale {
            self.contacts.remove(id);
            self.occupancy.insert(*id, 0);
        }
        before - self.contacts.len()
    }

    /// Build the current tactical picture.
    pub fn picture(&self) -> TacticalPicture {
        let contacts: Vec<PersonContact> = self.contacts.values().cloned().collect();
        let occupancy: Vec<(RoomId, u32)> =
            self.occupancy.iter().map(|(id, n)| (*id, *n)).collect();
        TacticalPicture::assemble(
            &self.structure.name,
            &self.structure.rooms,
            contacts,
            &occupancy,
        )
    }

    /// Localize a reading to `(x, y, point_fix, uncertainty_radius)`.
    fn localize(&self, room: &Room, reading: &RoomReading) -> (f64, f64, bool, f64) {
        if !reading.sensor_rssi.is_empty() {
            let zone = room.to_scan_zone(&reading.sensor_rssi);
            if let Some(pos) = self.localizer.estimate_position(&reading.vitals, &zone) {
                return (pos.x, pos.y, true, pos.uncertainty.horizontal_error);
            }
        }
        // Fall back to room-level presence: centroid + enclosing radius.
        let (cx, cy) = room.bounds.center();
        (cx, cy, false, room.bounds.enclosing_radius())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::reading::{MovementLevel, ReadingInput, SensorRssi};
    use crate::domain::structure::{Room, RoomBounds};

    fn structure_with_triangulable_room() -> (Structure, RoomId) {
        let room = Room::new("Den", 0, RoomBounds::new(0.0, 0.0, 12.0, 12.0))
            .with_sensor("s1", 0.0, 0.0)
            .with_sensor("s2", 12.0, 0.0)
            .with_sensor("s3", 6.0, 12.0);
        let id = room.id;
        (Structure::new("Test House").with_room(room), id)
    }

    fn reading_for(id: RoomId, rssi: Vec<SensorRssi>) -> RoomReading {
        let input = ReadingInput {
            room_id: Some(id),
            room_name: None,
            presence: true,
            breathing_bpm: Some(16.0),
            movement: MovementLevel::None,
            occupancy: Some(1),
            sensor_rssi: rssi.clone(),
        };
        RoomReading {
            room_id: id,
            vitals: input.to_vitals().unwrap(),
            occupancy: input.effective_occupancy(),
            sensor_rssi: rssi.into_iter().map(|s| (s.id, s.rssi)).collect(),
        }
    }

    #[test]
    fn unknown_room_is_rejected() {
        let (structure, _) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        let bogus = RoomId::new();
        let reading = reading_for(bogus, vec![]);
        assert!(matches!(
            engine.ingest(reading),
            Err(TacticalError::UnknownRoom(_))
        ));
    }

    #[test]
    fn room_level_presence_when_no_rssi() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        engine.ingest(reading_for(id, vec![])).unwrap();
        let pic = engine.picture();
        assert_eq!(pic.contacts.len(), 1);
        let c = &pic.contacts[0];
        assert!(!c.point_fix, "no RSSI => centroid fallback, not a point fix");
        assert!((c.x - 6.0).abs() < 1e-6 && (c.y - 6.0).abs() < 1e-6);
        assert_eq!(pic.total_occupancy_estimate, 1);
        assert_eq!(pic.occupied_rooms, 1);
    }

    #[test]
    fn triangulated_point_fix_when_rssi_present() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        let rssi = vec![
            SensorRssi { id: "s1".into(), rssi: -55.0 },
            SensorRssi { id: "s2".into(), rssi: -60.0 },
            SensorRssi { id: "s3".into(), rssi: -58.0 },
        ];
        engine.ingest(reading_for(id, rssi)).unwrap();
        let pic = engine.picture();
        assert_eq!(pic.contacts.len(), 1);
        assert!(pic.contacts[0].point_fix, "3 RSSI sensors => point fix");
    }

    #[test]
    fn contact_id_is_stable_across_updates() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        engine.ingest(reading_for(id, vec![])).unwrap();
        let first = engine.picture().contacts[0].id;
        engine.ingest(reading_for(id, vec![])).unwrap();
        let second = engine.picture().contacts[0].id;
        assert_eq!(first, second, "same room keeps the same contact id");
    }

    #[test]
    fn absence_clears_the_room() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        engine.ingest(reading_for(id, vec![])).unwrap();
        assert_eq!(engine.picture().contacts.len(), 1);
        engine.record_absence(id).unwrap();
        let pic = engine.picture();
        assert_eq!(pic.contacts.len(), 0);
        assert_eq!(pic.total_occupancy_estimate, 0);
    }

    #[test]
    fn stale_contacts_are_pruned() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        engine.ingest(reading_for(id, vec![])).unwrap();
        assert_eq!(engine.picture().contacts.len(), 1);
        // Cutoff in the far future => everything is stale.
        let removed = engine.prune_stale_before(Utc::now() + Duration::seconds(3600));
        assert_eq!(removed, 1);
        assert_eq!(engine.picture().contacts.len(), 0);
    }
}
