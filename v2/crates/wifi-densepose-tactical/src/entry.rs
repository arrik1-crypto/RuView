//! Entry advisor.
//!
//! Turns a [`TacticalPicture`] into a prioritized, plain-language set of room
//! assessments an element leader can read at a glance. It **organizes what the
//! sensors saw**; it does not authorize entry, rank threats, or identify anyone.
//! Every field is advisory and must be corroborated.

use serde::{Deserialize, Serialize};

use crate::domain::contact::{LifeSign, Motion, PersonContact};
use crate::domain::picture::{RoomOccupancy, TacticalPicture};
use crate::domain::structure::RoomId;

/// Relative attention a room warrants, based purely on presence + motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TacticalPriority {
    /// Stationary occupant(s) detected — a place people are staying put. In a
    /// hostage context this is where held persons *or* a barricaded subject may
    /// be; the sensor cannot tell which.
    Focus,
    /// Moving contact(s) — a mobile subject; expect the picture to change.
    Active,
    /// Weak / borderline signature — worth watching, not confirmed.
    Monitor,
    /// No presence detected. Does NOT confirm the room is empty.
    NoContact,
}

impl TacticalPriority {
    /// Sort key (lower sorts first): Focus, then Active, then Monitor, then NoContact.
    fn rank(&self) -> u8 {
        match self {
            TacticalPriority::Focus => 0,
            TacticalPriority::Active => 1,
            TacticalPriority::Monitor => 2,
            TacticalPriority::NoContact => 3,
        }
    }

    /// Lowercase tag for UI / JSON.
    pub fn tag(&self) -> &'static str {
        match self {
            TacticalPriority::Focus => "focus",
            TacticalPriority::Active => "active",
            TacticalPriority::Monitor => "monitor",
            TacticalPriority::NoContact => "no_contact",
        }
    }
}

/// Assessment of a single room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomAssessment {
    /// Room id.
    pub room_id: RoomId,
    /// Room label.
    pub room_name: String,
    /// Storey.
    pub floor: i32,
    /// Assigned priority.
    pub priority: TacticalPriority,
    /// Occupancy estimate for the room (lower bound).
    pub occupancy_estimate: u32,
    /// Whether the room is point-localizable (>= 3 sensors).
    pub localizable: bool,
    /// Plain-language rationale, safe to read aloud.
    pub rationale: String,
}

/// The full advisory: ordered room assessments plus a summary and disclaimer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryRecommendation {
    /// One-line situation summary.
    pub summary: String,
    /// Room assessments, highest-attention first.
    pub rooms: Vec<RoomAssessment>,
    /// Standing disclaimer, carried in the payload so no viewer can drop it.
    pub disclaimer: String,
}

/// The disclaimer stamped on every recommendation.
pub const ENTRY_DISCLAIMER: &str =
    "ADVISORY ONLY. This ordering reflects sensed presence and motion, NOT threat, \
     identity, or friend/foe. WiFi sensing cannot distinguish hostage from suspect, \
     cannot confirm a weapon, and cannot prove a room is empty. Not a substitute for \
     command judgment or a use-of-force decision.";

/// Produces [`EntryRecommendation`]s from a [`TacticalPicture`].
pub struct EntryAdvisor;

impl EntryAdvisor {
    /// Assess a picture. Pure function of the input snapshot.
    pub fn assess(picture: &TacticalPicture) -> EntryRecommendation {
        let mut rooms: Vec<RoomAssessment> = picture
            .rooms
            .iter()
            .map(|room| {
                let in_room: Vec<_> = picture
                    .contacts
                    .iter()
                    .filter(|c| c.room_id == room.room_id)
                    .collect();

                let mmwave_present =
                    room.mmwave.as_ref().map(|m| m.presence).unwrap_or(false);

                let priority = if !in_room.is_empty() {
                    if in_room.iter().any(|c| c.motion == Motion::Moving) {
                        TacticalPriority::Active
                    } else if in_room.iter().all(|c| c.life_sign == LifeSign::Faint) {
                        TacticalPriority::Monitor
                    } else {
                        TacticalPriority::Focus
                    }
                } else if mmwave_present {
                    // Independent radar detects a person the CSI mesh cannot see
                    // (e.g. perfectly still, behind heavy construction). This is a
                    // real positive detection — it must never read as NoContact,
                    // and in a rescue context it warrants top attention.
                    TacticalPriority::Focus
                } else {
                    TacticalPriority::NoContact
                };

                let rationale = Self::rationale(priority, &in_room, room);

                RoomAssessment {
                    room_id: room.room_id,
                    room_name: room.room_name.clone(),
                    floor: room.floor,
                    priority,
                    occupancy_estimate: room.occupancy_estimate,
                    localizable: room.localizable,
                    rationale,
                }
            })
            .collect();

        rooms.sort_by(|a, b| {
            a.priority
                .rank()
                .cmp(&b.priority.rank())
                .then_with(|| b.occupancy_estimate.cmp(&a.occupancy_estimate))
        });

        EntryRecommendation {
            summary: Self::summary(picture),
            rooms,
            disclaimer: ENTRY_DISCLAIMER.to_string(),
        }
    }

    fn summary(picture: &TacticalPicture) -> String {
        let mmwave_only = picture
            .rooms
            .iter()
            .filter(|r| {
                r.contact_count == 0 && r.mmwave.as_ref().map(|m| m.presence).unwrap_or(false)
            })
            .count();

        if picture.occupied_rooms == 0 && mmwave_only == 0 {
            return "No presence detected anywhere in the structure. This does NOT \
                    confirm it is empty — verify by other means."
                .to_string();
        }

        let mut s = format!(
            "~{} person(s) sensed by CSI across {} room(s). Presence and motion \
             only — no identity or threat assessment.",
            picture.total_occupancy_estimate, picture.occupied_rooms
        );
        if mmwave_only > 0 {
            s.push_str(&format!(
                " PLUS {mmwave_only} room(s) where mmWave radar detects a person the \
                 CSI mesh does NOT — investigate."
            ));
        }
        s
    }

    fn rationale(
        priority: TacticalPriority,
        in_room: &[&PersonContact],
        room: &RoomOccupancy,
    ) -> String {
        let occupancy = room.occupancy_estimate;
        let loc = if room.localizable {
            "point-localized"
        } else {
            "room-level presence only (add sensors for a point fix)"
        };

        match priority {
            TacticalPriority::NoContact => {
                "No contact. Absence is not proof of an empty room.".to_string()
            }
            TacticalPriority::Active => format!(
                "~{occupancy} contact(s), at least one MOVING — mobile subject, \
                 picture will change; {loc}."
            ),
            // mmWave-only positive detection with no CSI contact.
            TacticalPriority::Focus if in_room.is_empty() => {
                let vitals = room
                    .mmwave
                    .as_ref()
                    .and_then(|m| m.breathing_bpm)
                    .map(|b| format!(", ~{} br", b.round() as i32))
                    .unwrap_or_default();
                format!(
                    "mmWave radar detects a person here{vitals} that the CSI mesh does \
                     NOT — likely very still and/or behind heavy construction. Corroborate."
                )
            }
            TacticalPriority::Focus => {
                let breathing = in_room
                    .iter()
                    .any(|c| c.life_sign == LifeSign::BreathingConfirmed);
                let life = if breathing {
                    "breathing confirmed"
                } else {
                    "movement-based presence"
                };
                // Report the actual motion mix, not a hardcoded "stationary".
                let any_restless = in_room.iter().any(|c| c.motion == Motion::Restless);
                let motion = if any_restless {
                    "mostly stationary (some fidgeting)"
                } else {
                    "stationary"
                };
                let corr = if room.corroborated {
                    " [mmWave corroborated]"
                } else {
                    ""
                };
                format!(
                    "~{occupancy} {motion} contact(s), {life} — people staying put \
                     (held persons or barricaded subject; sensor cannot tell); {loc}.{corr}"
                )
            }
            TacticalPriority::Monitor => format!(
                "~{occupancy} faint/borderline signature(s) — watch, not confirmed; {loc}."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::reading::{MovementLevel, ReadingInput};
    use crate::domain::structure::{Room, RoomBounds, Structure};
    use crate::engine::TacticalEngine;

    fn engine_with_two_rooms() -> (TacticalEngine, RoomId, RoomId) {
        let bedroom = Room::new("Bedroom", 0, RoomBounds::new(0.0, 0.0, 4.0, 4.0));
        let hallway = Room::new("Hallway", 0, RoomBounds::new(4.0, 0.0, 6.0, 8.0));
        let (bid, hid) = (bedroom.id, hallway.id);
        let structure = Structure::new("Test").with_room(bedroom).with_room(hallway);
        (TacticalEngine::new(structure), bid, hid)
    }

    fn ingest(engine: &mut TacticalEngine, id: RoomId, movement: MovementLevel, breathing: Option<f32>) {
        let input = ReadingInput {
            room_id: Some(id),
            room_name: None,
            presence: true,
            breathing_bpm: breathing,
            movement,
            occupancy: Some(1),
            sensor_rssi: vec![],
        };
        let reading = crate::domain::reading::RoomReading {
            room_id: id,
            vitals: input.to_vitals().unwrap(),
            occupancy: input.effective_occupancy(),
            sensor_rssi: vec![],
        };
        engine.ingest(reading).unwrap();
    }

    #[test]
    fn mmwave_only_presence_is_not_no_contact() {
        let (mut engine, bid, _) = engine_with_two_rooms();
        // Radar sees a still person the CSI mesh missed; no CSI contact created.
        engine
            .apply_mmwave(&crate::domain::reading::MmwaveReading {
                room_id: Some(bid),
                room_name: None,
                presence: true,
                breathing_bpm: Some(14.0),
                heart_rate_bpm: Some(65.0),
                distance_cm: Some(280.0),
                targets: 1,
                confidence: 85,
            })
            .unwrap();
        let rec = EntryAdvisor::assess(&engine.picture());
        let bedroom = rec.rooms.iter().find(|r| r.room_id == bid).unwrap();
        assert_ne!(bedroom.priority, TacticalPriority::NoContact, "mmWave presence must not read as NoContact");
        assert_eq!(bedroom.priority, TacticalPriority::Focus);
        assert!(rec.summary.contains("mmWave"), "summary must surface mmWave-only detection");
        assert!(bedroom.rationale.contains("mmWave"));
    }

    #[test]
    fn stationary_room_focuses_before_moving_room() {
        let (mut engine, bid, hid) = engine_with_two_rooms();
        // Bedroom: stationary breathing (a held-position occupant).
        ingest(&mut engine, bid, MovementLevel::None, Some(16.0));
        // Hallway: gross movement (a mobile subject).
        ingest(&mut engine, hid, MovementLevel::Gross, None);

        let rec = EntryAdvisor::assess(&engine.picture());
        assert_eq!(rec.rooms[0].priority, TacticalPriority::Focus);
        assert_eq!(rec.rooms[0].room_name, "Bedroom");
        assert_eq!(rec.rooms[1].priority, TacticalPriority::Active);
        assert!(rec.disclaimer.contains("ADVISORY ONLY"));
    }

    #[test]
    fn empty_structure_summary_is_honest() {
        let (engine, _, _) = engine_with_two_rooms();
        let rec = EntryAdvisor::assess(&engine.picture());
        assert!(rec.summary.contains("No presence"));
        assert!(rec.rooms.iter().all(|r| r.priority == TacticalPriority::NoContact));
    }
}
