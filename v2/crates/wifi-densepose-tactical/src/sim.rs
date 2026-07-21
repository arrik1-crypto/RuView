//! Hardware-free scenario simulator.
//!
//! Everything this module produces is **SYNTHETIC** — there is no live radio.
//! It exists so the app is runnable and demonstrable end-to-end without an ESP32
//! mesh, and so the UI and engine can be exercised. Do not mistake simulated
//! contacts for real ones.

use crate::domain::reading::{MovementLevel, ReadingInput, SensorRssi};
use crate::domain::structure::{Room, RoomBounds, RoomId, Structure};

/// A small single-storey demo house with realistic sensor placement.
///
/// One bedroom is instrumented with three nodes (triangulable → point fixes);
/// the rest have one or two (room-level presence only), mirroring a real
/// budget-constrained deployment.
pub fn demo_structure() -> Structure {
    let living = Room::new("Living Room", 0, RoomBounds::new(0.0, 0.0, 6.0, 5.0))
        .with_sensor("living-a", 0.2, 0.2)
        .with_sensor("living-b", 5.8, 0.2);
    let kitchen = Room::new("Kitchen", 0, RoomBounds::new(6.0, 0.0, 10.0, 5.0))
        .with_sensor("kitchen-a", 6.2, 4.8);
    let hallway = Room::new("Hallway", 0, RoomBounds::new(0.0, 5.0, 10.0, 6.5))
        .with_sensor("hall-a", 0.2, 5.8)
        .with_sensor("hall-b", 9.8, 5.8);
    let bedroom1 = Room::new("Bedroom 1", 0, RoomBounds::new(0.0, 6.5, 5.0, 11.0))
        .with_sensor("bed1-a", 0.2, 6.7)
        .with_sensor("bed1-b", 4.8, 6.7)
        .with_sensor("bed1-c", 2.5, 10.8);
    let bedroom2 = Room::new("Bedroom 2", 0, RoomBounds::new(5.0, 6.5, 10.0, 11.0))
        .with_sensor("bed2-a", 5.2, 6.7)
        .with_sensor("bed2-b", 9.8, 6.7)
        .with_sensor("bed2-c", 7.5, 10.8);
    let bathroom = Room::new("Bathroom", 0, RoomBounds::new(10.0, 0.0, 12.0, 6.5))
        .with_sensor("bath-a", 11.0, 3.0);

    Structure::new("SIM — 1420 Cedar St")
        .with_room(living)
        .with_room(kitchen)
        .with_room(hallway)
        .with_room(bedroom1)
        .with_room(bedroom2)
        .with_room(bathroom)
}

/// Ground-truth occupancy for a room in the scripted scenario.
#[derive(Debug, Clone)]
struct RoomTruth {
    room_id: RoomId,
    /// People present (0 = empty).
    occupancy: u32,
    /// Whether occupants move around.
    moving: bool,
    /// Whether a breathing rhythm is resolvable.
    breathing: bool,
}

/// A scripted, deterministic scenario over the demo structure.
///
/// Scenario: two people held together, still and breathing, in **Bedroom 2**
/// (the triangulable room); a single mobile subject pacing the **Hallway**;
/// everything else empty. This is the canonical hostage-room layout the app is
/// meant to make legible.
pub struct Scenario {
    structure: Structure,
    truth: Vec<RoomTruth>,
    tick: u64,
    rng: u64,
}

impl Default for Scenario {
    fn default() -> Self {
        Self::new()
    }
}

impl Scenario {
    /// Build the scripted scenario.
    pub fn new() -> Self {
        let structure = demo_structure();
        let id = |name: &str| structure.room_by_name(name).unwrap().id;
        let truth = vec![
            RoomTruth { room_id: id("Living Room"), occupancy: 0, moving: false, breathing: false },
            RoomTruth { room_id: id("Kitchen"), occupancy: 0, moving: false, breathing: false },
            RoomTruth { room_id: id("Hallway"), occupancy: 1, moving: true, breathing: false },
            RoomTruth { room_id: id("Bedroom 1"), occupancy: 0, moving: false, breathing: false },
            RoomTruth { room_id: id("Bedroom 2"), occupancy: 2, moving: false, breathing: true },
            RoomTruth { room_id: id("Bathroom"), occupancy: 0, moving: false, breathing: false },
        ];
        Self { structure, truth, tick: 0, rng: 0x5EED_1234 }
    }

    /// The structure this scenario runs over (load it into the engine).
    pub fn structure(&self) -> &Structure {
        &self.structure
    }

    /// Advance one sensing cycle and emit a reading per room.
    ///
    /// Occupied rooms emit presence with small jitter and the occasional missed
    /// cycle (radios are noisy); empty rooms emit `presence: false`.
    pub fn tick(&mut self) -> Vec<ReadingInput> {
        self.tick += 1;
        let truth = self.truth.clone();
        truth
            .iter()
            .map(|t| self.reading_for(t))
            .collect()
    }

    fn reading_for(&mut self, t: &RoomTruth) -> ReadingInput {
        // A room's nodes report every cycle (even when empty — a live node still
        // says "no one here"), except on an occasional dropout (~1 in 14) where
        // the whole room's nodes go quiet, exercising the sensor-status panel.
        let dropout = self.next_u32() % 14 == 0;
        let sensor_rssi = if dropout {
            vec![]
        } else {
            self.rssi_for_room(t.room_id)
        };

        // Empty room, or a dropout cycle: no presence.
        if t.occupancy == 0 || dropout {
            return ReadingInput {
                room_id: Some(t.room_id),
                room_name: None,
                presence: false,
                breathing_bpm: None,
                movement: MovementLevel::None,
                occupancy: Some(0),
                sensor_rssi,
            };
        }

        let movement = if t.moving {
            MovementLevel::Gross
        } else if self.next_u32() % 4 == 0 {
            // Still occupants show intermittent micro-motion.
            MovementLevel::Fine
        } else {
            MovementLevel::None
        };

        let breathing_bpm = if t.breathing {
            // 14–18 bpm with jitter.
            Some(14.0 + (self.next_u32() % 40) as f32 / 10.0)
        } else {
            None
        };

        ReadingInput {
            room_id: Some(t.room_id),
            room_name: None,
            presence: true,
            breathing_bpm,
            movement,
            occupancy: Some(t.occupancy),
            sensor_rssi,
        }
    }

    /// Emit plausible per-sensor RSSI for every node in the room, so
    /// triangulable rooms produce point fixes.
    fn rssi_for_room(&mut self, room_id: RoomId) -> Vec<SensorRssi> {
        let ids: Vec<String> = self
            .structure
            .room(room_id)
            .map(|r| r.sensors.iter().map(|s| s.id.clone()).collect())
            .unwrap_or_default();
        ids.into_iter()
            .map(|id| {
                // -52 .. -66 dBm with jitter.
                let jitter = (self.next_u32() % 140) as f64 / 10.0;
                SensorRssi { id, rssi: -52.0 - jitter }
            })
            .collect()
    }

    /// Deterministic xorshift-ish LCG — reproducible synthetic noise without a
    /// dependency on `rand` or wall-clock entropy.
    fn next_u32(&mut self) -> u32 {
        // Numerical Recipes LCG constants.
        self.rng = self.rng.wrapping_mul(1664525).wrapping_add(1013904223);
        (self.rng >> 16) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_structure_has_a_triangulable_room() {
        let s = demo_structure();
        assert!(s.room_by_name("Bedroom 2").unwrap().localizable());
        assert_eq!(s.rooms.len(), 6);
    }

    #[test]
    fn scenario_emits_one_reading_per_room() {
        let mut scenario = Scenario::new();
        let readings = scenario.tick();
        assert_eq!(readings.len(), 6);
    }

    #[test]
    fn scenario_is_deterministic() {
        let mut a = Scenario::new();
        let mut b = Scenario::new();
        for _ in 0..5 {
            let ra = a.tick();
            let rb = b.tick();
            let pa: Vec<bool> = ra.iter().map(|r| r.presence).collect();
            let pb: Vec<bool> = rb.iter().map(|r| r.presence).collect();
            assert_eq!(pa, pb, "same seed must produce identical scenarios");
        }
    }

    #[test]
    fn occupied_rooms_are_usually_present() {
        let mut scenario = Scenario::new();
        let bed2 = scenario.structure().room_by_name("Bedroom 2").unwrap().id;
        let mut present = 0;
        for _ in 0..30 {
            for r in scenario.tick() {
                if r.room_id == Some(bed2) && r.presence {
                    present += 1;
                }
            }
        }
        // With ~1/12 dropout, the held room is present the large majority of ticks.
        assert!(present > 20, "expected mostly-present held room, got {present}/30");
    }
}
