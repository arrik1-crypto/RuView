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
    pub fn assemble(
        structure_name: &str,
        rooms: &[Room],
        contacts: Vec<PersonContact>,
        occupancy_by_room: &[(RoomId, u32)],
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
                }
            })
            .collect();

        let total_occupancy_estimate = room_rollup.iter().map(|r| r.occupancy_estimate).sum();
        let occupied_rooms = room_rollup.iter().filter(|r| r.contact_count > 0).count();

        Self {
            structure_name: structure_name.to_string(),
            generated_at: Utc::now(),
            rooms: room_rollup,
            contacts,
            total_occupancy_estimate,
            occupied_rooms,
            advisory: STANDING_ADVISORY.to_string(),
        }
    }
}
