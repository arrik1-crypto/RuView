//! Inputs to the engine: what a sensing cycle observed for one room.
//!
//! [`ReadingInput`] is the simple, hardware-agnostic shape a caller (live CSI
//! pipeline, replay, or the simulator) posts to the API. [`RoomReading`] is the
//! resolved internal form, carrying a MAT [`VitalSignsReading`].

use serde::{Deserialize, Serialize};
use wifi_densepose_mat::domain::{
    BreathingPattern, BreathingType, MovementProfile, MovementType, VitalSignsReading,
};

use super::structure::RoomId;

/// Coarse movement level reported for a room in a sensing cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovementLevel {
    /// No motion energy above the noise floor.
    None,
    /// Small / intermittent motion.
    Fine,
    /// Sustained gross motion.
    Gross,
}

impl MovementLevel {
    fn to_profile(self) -> MovementProfile {
        match self {
            MovementLevel::None => MovementProfile::default(),
            MovementLevel::Fine => MovementProfile {
                movement_type: MovementType::Fine,
                intensity: 0.4,
                frequency: 0.8,
                is_voluntary: false,
            },
            MovementLevel::Gross => MovementProfile {
                movement_type: MovementType::Gross,
                intensity: 0.8,
                frequency: 1.2,
                is_voluntary: true,
            },
        }
    }
}

/// A live RSSI reading from one sensor node, toward the current detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorRssi {
    /// Node id (matches a [`super::structure::SensorPlacement::id`]).
    pub id: String,
    /// Measured signal strength (dBm).
    pub rssi: f64,
}

/// Hardware-agnostic reading posted for one room.
///
/// A caller distils a sensing cycle down to: is anyone present, at what
/// breathing rate (if resolved), how much movement, roughly how many, and the
/// per-sensor RSSI (if a multi-node deployment can triangulate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadingInput {
    /// Room this reading is for (by id). Prefer this when known.
    #[serde(default)]
    pub room_id: Option<RoomId>,
    /// Room this reading is for (by name). Used when `room_id` is absent.
    #[serde(default)]
    pub room_name: Option<String>,
    /// Whether any human presence was detected at all.
    pub presence: bool,
    /// Resolved breathing rate (breaths/min), if a breathing rhythm was found.
    #[serde(default)]
    pub breathing_bpm: Option<f32>,
    /// Movement level observed.
    #[serde(default = "default_movement")]
    pub movement: MovementLevel,
    /// Best occupancy estimate (min number of people). Defaults to 1 when
    /// presence is detected. WiFi occupancy counting is coarse — treat as ">=".
    #[serde(default)]
    pub occupancy: Option<u32>,
    /// Per-sensor RSSI for triangulation, if available.
    #[serde(default)]
    pub sensor_rssi: Vec<SensorRssi>,
}

fn default_movement() -> MovementLevel {
    MovementLevel::None
}

impl ReadingInput {
    /// Build the MAT [`VitalSignsReading`] this input implies, or `None` if no
    /// presence was detected.
    pub fn to_vitals(&self) -> Option<VitalSignsReading> {
        if !self.presence {
            return None;
        }
        let breathing = self.breathing_bpm.map(|bpm| BreathingPattern {
            rate_bpm: bpm,
            // Amplitude/regularity are not carried by the coarse input; assume a
            // moderate, usable signal. Confidence downstream still reflects this.
            amplitude: 0.7,
            regularity: 0.75,
            pattern_type: classify_breathing(bpm),
        });
        let movement = self.movement.to_profile();
        // Guard: with neither breathing nor movement there is nothing to report,
        // even if `presence` was optimistically set.
        if breathing.is_none() && movement.movement_type == MovementType::None {
            return None;
        }
        Some(VitalSignsReading::new(breathing, None, movement))
    }

    /// Occupancy to assume, clamped to at least 1 when presence is detected.
    pub fn effective_occupancy(&self) -> u32 {
        self.occupancy.unwrap_or(1).max(1)
    }
}

fn classify_breathing(bpm: f32) -> BreathingType {
    if bpm < 10.0 {
        BreathingType::Shallow
    } else if bpm > 30.0 {
        BreathingType::Labored
    } else {
        BreathingType::Normal
    }
}

/// Resolved reading: a room id plus the vital-signs and RSSI it produced.
#[derive(Debug, Clone)]
pub struct RoomReading {
    /// Room this reading applies to.
    pub room_id: RoomId,
    /// Detected vitals (already presence-guarded).
    pub vitals: VitalSignsReading,
    /// Occupancy estimate (>= 1).
    pub occupancy: u32,
    /// Per-sensor RSSI as `(id, dbm)` for MAT localization.
    pub sensor_rssi: Vec<(String, f64)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_presence_yields_no_vitals() {
        let input = ReadingInput {
            room_id: None,
            room_name: Some("Den".into()),
            presence: false,
            breathing_bpm: Some(15.0),
            movement: MovementLevel::Gross,
            occupancy: Some(2),
            sensor_rssi: vec![],
        };
        assert!(input.to_vitals().is_none());
    }

    #[test]
    fn presence_with_breathing_builds_reading() {
        let input = ReadingInput {
            room_id: None,
            room_name: Some("Den".into()),
            presence: true,
            breathing_bpm: Some(16.0),
            movement: MovementLevel::None,
            occupancy: None,
            sensor_rssi: vec![],
        };
        let v = input.to_vitals().expect("should have vitals");
        assert!(v.has_breathing());
        assert_eq!(input.effective_occupancy(), 1);
    }

    #[test]
    fn presence_but_empty_signal_is_none() {
        let input = ReadingInput {
            room_id: None,
            room_name: Some("Den".into()),
            presence: true,
            breathing_bpm: None,
            movement: MovementLevel::None,
            occupancy: None,
            sensor_rssi: vec![],
        };
        assert!(input.to_vitals().is_none());
    }
}
