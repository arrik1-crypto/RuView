//! Entry advisor.
//!
//! Turns a [`TacticalPicture`] into a prioritized, plain-language set of room
//! assessments an element leader can read at a glance. It **organizes what the
//! sensors saw**; it does not authorize entry, rank threats, or identify anyone.
//! Every field is advisory and must be corroborated.

use serde::{Deserialize, Serialize};

use crate::domain::contact::{LifeSign, Motion};
use crate::domain::picture::TacticalPicture;
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

                let priority = if in_room.is_empty() {
                    TacticalPriority::NoContact
                } else if in_room.iter().any(|c| c.motion == Motion::Moving) {
                    TacticalPriority::Active
                } else if in_room
                    .iter()
                    .all(|c| c.life_sign == LifeSign::Faint)
                {
                    TacticalPriority::Monitor
                } else {
                    TacticalPriority::Focus
                };

                let rationale = Self::rationale(priority, &in_room, room.occupancy_estimate, room.localizable);

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
        if picture.occupied_rooms == 0 {
            return "No presence detected anywhere in the structure. This does NOT \
                    confirm it is empty — verify by other means."
                .to_string();
        }
        format!(
            "~{} person(s) sensed across {} room(s). Presence and motion only — \
             no identity or threat assessment.",
            picture.total_occupancy_estimate, picture.occupied_rooms
        )
    }

    fn rationale(
        priority: TacticalPriority,
        in_room: &[&crate::domain::contact::PersonContact],
        occupancy: u32,
        localizable: bool,
    ) -> String {
        let loc = if localizable {
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
            TacticalPriority::Focus => {
                let breathing = in_room
                    .iter()
                    .any(|c| c.life_sign == LifeSign::BreathingConfirmed);
                let life = if breathing {
                    "breathing confirmed"
                } else {
                    "movement-based presence"
                };
                format!(
                    "~{occupancy} STATIONARY contact(s), {life} — people staying put \
                     (held persons or barricaded subject; sensor cannot tell); {loc}."
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
