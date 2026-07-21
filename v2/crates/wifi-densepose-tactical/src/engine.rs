//! The tactical engine: ingest readings, localize, track contacts, emit pictures.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use wifi_densepose_mat::localization::LocalizationService;

use crate::domain::contact::{ContactId, PersonContact};
use crate::domain::picture::{MmwaveCorroboration, SensorSummary, TacticalPicture};
use crate::domain::reading::{MmwaveReading, RoomReading};
use crate::domain::structure::{Room, RoomId, Structure};
use crate::error::TacticalError;

/// Last-heard state for one sensor node.
#[derive(Debug, Clone)]
struct SensorActivity {
    room_id: RoomId,
    last_seen: DateTime<Utc>,
    last_rssi: Option<f64>,
}

/// Last mmWave reading for a room.
#[derive(Debug, Clone)]
struct MmwaveState {
    presence: bool,
    breathing_bpm: Option<f32>,
    heart_rate_bpm: Option<f32>,
    distance_cm: Option<f32>,
    targets: u8,
    confidence: u8,
    last_seen: DateTime<Utc>,
}

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
    /// A sensor node not heard from within this many seconds is reported as not
    /// reporting (a dropout) in the sensor status roll-up.
    pub sensor_stale_secs: i64,
    /// An mmWave reading older than this many seconds is dropped from the picture.
    pub mmwave_stale_secs: i64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            position_alpha: 0.5,
            contact_ttl_secs: 8,
            sensor_stale_secs: 6,
            mmwave_stale_secs: 6,
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
    /// Last-heard state per sensor node id.
    sensors: HashMap<String, SensorActivity>,
    /// Latest mmWave reading per room.
    mmwave: HashMap<RoomId, MmwaveState>,
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
            sensors: HashMap::new(),
            mmwave: HashMap::new(),
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
        self.sensors.clear();
        self.mmwave.clear();
    }

    /// Ingest an mmWave (MR60BHA2) reading: resolve its room (by id or name) and
    /// store it as the room's latest corroboration reading.
    pub fn apply_mmwave(&mut self, reading: &MmwaveReading) -> Result<(), TacticalError> {
        let room_id = self
            .resolve_room(reading.room_id, reading.room_name.as_deref())
            .ok_or_else(|| {
                TacticalError::UnknownRoom(
                    reading
                        .room_name
                        .clone()
                        .or_else(|| reading.room_id.map(|id| id.to_string()))
                        .unwrap_or_else(|| "unspecified".to_string()),
                )
            })?;
        self.mmwave.insert(
            room_id,
            MmwaveState {
                presence: reading.presence,
                // Only carry a rate the radar actually resolved (> 0).
                breathing_bpm: reading.breathing_bpm.filter(|v| *v > 0.0),
                heart_rate_bpm: reading.heart_rate_bpm.filter(|v| *v > 0.0),
                distance_cm: reading.distance_cm.filter(|v| *v > 0.0),
                targets: reading.targets,
                confidence: reading.confidence,
                last_seen: Utc::now(),
            },
        );
        Ok(())
    }

    /// Per-room mmWave corroboration, dropping readings past the freshness window.
    fn mmwave_report(&self) -> Vec<(RoomId, MmwaveCorroboration)> {
        self.mmwave_report_at(Utc::now())
    }

    /// mmWave report against an explicit `now` (testable without wall-clock waits).
    pub fn mmwave_report_at(&self, now: DateTime<Utc>) -> Vec<(RoomId, MmwaveCorroboration)> {
        let window = self.config.mmwave_stale_secs;
        self.mmwave
            .iter()
            .filter_map(|(room_id, m)| {
                let age = (now - m.last_seen).num_seconds();
                if !(0..=window).contains(&age) {
                    return None;
                }
                Some((
                    *room_id,
                    MmwaveCorroboration {
                        presence: m.presence,
                        breathing_bpm: m.breathing_bpm,
                        heart_rate_bpm: m.heart_rate_bpm,
                        distance_cm: m.distance_cm,
                        targets: m.targets,
                        confidence: m.confidence,
                        age_secs: age,
                    },
                ))
            })
            .collect()
    }

    /// Record that a set of sensor nodes just reported (from a reading or CSI
    /// frame), refreshing their last-heard time and RSSI. Called on every live
    /// data path — including absences — so node health reflects link state, not
    /// whether a person was detected.
    pub fn note_sensors(&mut self, room_id: RoomId, rssi: &[(String, f64)]) {
        if rssi.is_empty() {
            return;
        }
        let now = Utc::now();
        for (id, r) in rssi {
            self.sensors.insert(
                id.clone(),
                SensorActivity {
                    room_id,
                    last_seen: now,
                    last_rssi: Some(*r),
                },
            );
        }
    }

    /// Per-node health roll-up: every configured node plus any unexpected nodes
    /// that reported, each flagged reporting/stale against the freshness window.
    pub fn sensor_report(&self) -> Vec<SensorSummary> {
        self.sensor_report_at(Utc::now())
    }

    /// Sensor report against an explicit `now` (testable without wall-clock waits).
    pub fn sensor_report_at(&self, now: DateTime<Utc>) -> Vec<SensorSummary> {
        let window = self.config.sensor_stale_secs;
        let mut out = Vec::new();
        let mut configured_ids = HashSet::new();

        for room in &self.structure.rooms {
            for s in &room.sensors {
                configured_ids.insert(s.id.clone());
                let (reporting, age, rssi) = match self.sensors.get(&s.id) {
                    Some(a) => {
                        let age = (now - a.last_seen).num_seconds();
                        ((0..=window).contains(&age), Some(age), a.last_rssi)
                    }
                    None => (false, None, None),
                };
                out.push(SensorSummary {
                    id: s.id.clone(),
                    room_id: Some(room.id),
                    room_name: Some(room.name.clone()),
                    configured: true,
                    reporting,
                    age_secs: age,
                    last_rssi: rssi,
                });
            }
        }

        // Nodes that reported but are not in the loaded floor plan.
        for (id, a) in &self.sensors {
            if !configured_ids.contains(id) {
                let age = (now - a.last_seen).num_seconds();
                out.push(SensorSummary {
                    id: id.clone(),
                    room_id: Some(a.room_id),
                    room_name: self.structure.room(a.room_id).map(|r| r.name.clone()),
                    configured: false,
                    reporting: (0..=window).contains(&age),
                    age_secs: Some(age),
                    last_rssi: a.last_rssi,
                });
            }
        }

        out.sort_by(|a, b| a.room_name.cmp(&b.room_name).then_with(|| a.id.cmp(&b.id)));
        out
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
        self.note_sensors(room.id, &reading.sensor_rssi);
        self.occupancy.insert(room.id, reading.occupancy.max(1));

        let (x, y, point_fix, uncertainty) = self.localize(&room, &reading);
        let motion = PersonContact::classify_motion(&reading.vitals);
        let life_sign = PersonContact::classify_life_sign(&reading.vitals);
        let breathing_bpm = reading.vitals.breathing.as_ref().map(|b| b.rate_bpm);
        let confidence = reading.vitals.confidence.value();

        match self.contacts.get_mut(&room.id) {
            Some(existing) => {
                // Only EMA-smooth when the fix TYPE is unchanged. On a transition
                // (room centroid <-> triangulated point fix) the old and new
                // coordinates mean different things; blending them would report a
                // position that its stated uncertainty_radius_m / point_fix no
                // longer describes (e.g. a tight 0.4 m "95% radius" around a point
                // halfway to the room centroid). Snap to the new fix instead.
                if existing.point_fix == point_fix {
                    let a = self.config.position_alpha;
                    existing.x = existing.x * (1.0 - a) + x * a;
                    existing.y = existing.y * (1.0 - a) + y * a;
                } else {
                    existing.x = x;
                    existing.y = y;
                }
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
        // Note node activity from every reading — including absences — so the
        // sensor roll-up tracks link state, not just detections.
        let rssi: Vec<(String, f64)> = input
            .sensor_rssi
            .iter()
            .map(|s| (s.id.clone(), s.rssi))
            .collect();
        self.note_sensors(room_id, &rssi);
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
            self.sensor_report(),
            &self.mmwave_report(),
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
    fn point_fix_after_centroid_snaps_not_blends() {
        let rssi = vec![
            SensorRssi { id: "s1".into(), rssi: -55.0 },
            SensorRssi { id: "s2".into(), rssi: -60.0 },
            SensorRssi { id: "s3".into(), rssi: -58.0 },
        ];
        // Engine A: a fresh point fix (no prior contact).
        let (sa, ida) = structure_with_triangulable_room();
        let mut a = TacticalEngine::new(sa);
        a.ingest(reading_for(ida, rssi.clone())).unwrap();
        let pa = a.picture().contacts[0].clone();
        // Engine B: room-level centroid first, THEN the same point fix.
        let (sb, idb) = structure_with_triangulable_room();
        let mut b = TacticalEngine::new(sb);
        b.ingest(reading_for(idb, vec![])).unwrap();
        b.ingest(reading_for(idb, rssi)).unwrap();
        let pb = b.picture().contacts[0].clone();
        assert!(pb.point_fix);
        // The transition must adopt the fresh point-fix position, not a blend of
        // the stale centroid — otherwise (x,y) wouldn't match the tight radius.
        assert!(
            (pa.x - pb.x).abs() < 1e-9 && (pa.y - pb.y).abs() < 1e-9,
            "centroid->point-fix must snap: fresh=({},{}) transitioned=({},{})",
            pa.x, pa.y, pb.x, pb.y
        );
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
    fn reporting_nodes_are_tracked() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        let rssi = vec![
            SensorRssi { id: "s1".into(), rssi: -55.0 },
            SensorRssi { id: "s2".into(), rssi: -60.0 },
            SensorRssi { id: "s3".into(), rssi: -58.0 },
        ];
        engine.ingest(reading_for(id, rssi)).unwrap();
        let pic = engine.picture();
        assert_eq!(pic.sensors_total, 3, "3 configured nodes");
        assert_eq!(pic.sensors_reporting, 3, "all 3 just reported");
        assert!(pic.sensors.iter().all(|s| s.reporting && s.last_rssi.is_some()));
    }

    #[test]
    fn nodes_go_stale_after_window() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        let rssi = vec![
            SensorRssi { id: "s1".into(), rssi: -55.0 },
            SensorRssi { id: "s2".into(), rssi: -60.0 },
            SensorRssi { id: "s3".into(), rssi: -58.0 },
        ];
        engine.ingest(reading_for(id, rssi)).unwrap();
        // Far-future cutoff => every node is past the freshness window.
        let report = engine.sensor_report_at(Utc::now() + Duration::seconds(3600));
        assert!(report.iter().all(|s| !s.reporting));
        assert!(report.iter().all(|s| s.configured));
    }

    #[test]
    fn configured_node_with_no_data_reads_not_reporting() {
        let (structure, _) = structure_with_triangulable_room();
        let engine = TacticalEngine::new(structure);
        let report = engine.sensor_report();
        assert_eq!(report.len(), 3);
        assert!(report.iter().all(|s| !s.reporting && s.age_secs.is_none()));
    }

    fn mmwave_reading(name: &str, presence: bool, br: Option<f32>) -> crate::domain::reading::MmwaveReading {
        crate::domain::reading::MmwaveReading {
            room_id: None,
            room_name: Some(name.into()),
            presence,
            breathing_bpm: br,
            heart_rate_bpm: Some(70.0),
            distance_cm: Some(300.0),
            targets: 1,
            confidence: 90,
        }
    }

    #[test]
    fn mmwave_corroborates_a_csi_contact() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        engine.ingest(reading_for(id, vec![])).unwrap(); // CSI contact
        engine.apply_mmwave(&mmwave_reading("Den", true, Some(15.0))).unwrap();
        let pic = engine.picture();
        let room = pic.rooms.iter().find(|r| r.room_id == id).unwrap();
        assert!(room.mmwave.is_some());
        assert!(room.corroborated, "CSI contact + mmWave presence => corroborated");
        assert_eq!(room.mmwave.as_ref().unwrap().breathing_bpm, Some(15.0));
    }

    #[test]
    fn mmwave_clear_with_csi_contact_is_not_corroborated() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        engine.ingest(reading_for(id, vec![])).unwrap();
        engine.apply_mmwave(&mmwave_reading("Den", false, None)).unwrap();
        let pic = engine.picture();
        let room = pic.rooms.iter().find(|r| r.room_id == id).unwrap();
        assert!(room.mmwave.is_some());
        assert!(!room.corroborated, "mmWave 'clear' must not corroborate");
    }

    #[test]
    fn stale_mmwave_drops_from_picture() {
        let (structure, id) = structure_with_triangulable_room();
        let mut engine = TacticalEngine::new(structure);
        engine.apply_mmwave(&mmwave_reading("Den", true, Some(15.0))).unwrap();
        // Fresh now: present.
        assert_eq!(engine.mmwave_report_at(Utc::now()).len(), 1);
        // Far-future: stale, dropped.
        assert!(engine.mmwave_report_at(Utc::now() + Duration::seconds(3600)).is_empty());
        let _ = id;
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
