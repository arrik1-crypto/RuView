//! A detected human presence inside the structure.
//!
//! A [`PersonContact`] is intentionally **anonymous and unclassified**. WiFi
//! sensing cannot tell a hostage from a hostage-taker, cannot identify who a
//! person is, and cannot confirm a weapon. The contact records only *what the
//! radio physically observed*: that a human is present, roughly where, whether
//! they are moving, and whether breathing was detected.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wifi_densepose_mat::domain::{MovementType, VitalSignsReading};

/// Stable identifier for a tracked [`PersonContact`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContactId(Uuid);

impl ContactId {
    /// Generate a fresh random id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ContactId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ContactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// How a contact is moving, as read from CSI motion energy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Motion {
    /// Detected but no bulk movement — a still occupant (seated, lying, holding
    /// position). Presence is inferred from breathing / micro-motion.
    Stationary,
    /// Small or intermittent movement (fidgeting, shifting weight, limb motion).
    Restless,
    /// Sustained gross movement — walking or repositioning through the space.
    Moving,
}

impl Motion {
    /// Lowercase tag for the UI / JSON consumers.
    pub fn tag(&self) -> &'static str {
        match self {
            Motion::Stationary => "stationary",
            Motion::Restless => "restless",
            Motion::Moving => "moving",
        }
    }
}

/// Confidence that the contact is a *living* person, from detected life signs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LifeSign {
    /// A breathing rhythm was resolved — strongest evidence of a live person.
    BreathingConfirmed,
    /// Movement seen but no clean breathing signal — presence without a
    /// confirmed life rhythm (could be a moving person, could be interference).
    MovementOnly,
    /// A weak / borderline signature. Treat as a possible contact, not a fact.
    Faint,
}

impl LifeSign {
    /// Lowercase tag for the UI / JSON consumers.
    pub fn tag(&self) -> &'static str {
        match self {
            LifeSign::BreathingConfirmed => "breathing",
            LifeSign::MovementOnly => "movement_only",
            LifeSign::Faint => "faint",
        }
    }
}

/// A single tracked human presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonContact {
    /// Stable id (persists across updates while the contact is tracked).
    pub id: ContactId,
    /// Room this contact is in.
    pub room_id: super::structure::RoomId,
    /// Room label, denormalized for display.
    pub room_name: String,
    /// Best-estimate floor-plan position (metres). `x`/`y` only; height is not
    /// resolved for tactical use.
    pub x: f64,
    /// Best-estimate floor-plan Y (metres).
    pub y: f64,
    /// `true` when the position is a real point fix (>= 3 sensors triangulated);
    /// `false` when it is only the room centroid (room-level presence).
    pub point_fix: bool,
    /// 95%-confidence radius around `(x, y)` in metres. Room-level contacts use
    /// the room's enclosing radius.
    pub uncertainty_radius_m: f64,
    /// Movement classification.
    pub motion: Motion,
    /// Life-sign classification.
    pub life_sign: LifeSign,
    /// Breathing rate in breaths/min, when resolved.
    pub breathing_bpm: Option<f32>,
    /// Detection confidence in `[0, 1]` (from the underlying vital-signs reading).
    pub confidence: f64,
    /// When this contact was first seen.
    pub first_seen: DateTime<Utc>,
    /// When this contact was last updated by a reading.
    pub last_updated: DateTime<Utc>,
}

impl PersonContact {
    /// Classify [`Motion`] from a MAT vital-signs reading. Gross voluntary motion
    /// reads as [`Motion::Moving`]; fine/tremor/periodic as [`Motion::Restless`];
    /// breathing-only presence as [`Motion::Stationary`].
    pub fn classify_motion(vitals: &VitalSignsReading) -> Motion {
        match vitals.movement.movement_type {
            MovementType::Gross => Motion::Moving,
            MovementType::Fine | MovementType::Tremor => Motion::Restless,
            // Periodic motion is usually breathing/chest movement, and "None"
            // means presence is carried by the breathing signal alone.
            MovementType::Periodic | MovementType::None => Motion::Stationary,
        }
    }

    /// Classify [`LifeSign`] from a MAT vital-signs reading.
    pub fn classify_life_sign(vitals: &VitalSignsReading) -> LifeSign {
        if vitals.has_breathing() {
            LifeSign::BreathingConfirmed
        } else if vitals.confidence.value() < 0.4 {
            LifeSign::Faint
        } else {
            LifeSign::MovementOnly
        }
    }

    /// Distance in the floor plane to a point (metres).
    pub fn distance_to(&self, x: f64, y: f64) -> f64 {
        ((self.x - x).powi(2) + (self.y - y).powi(2)).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wifi_densepose_mat::domain::{BreathingPattern, BreathingType, MovementProfile};

    fn breathing_reading() -> VitalSignsReading {
        VitalSignsReading::new(
            Some(BreathingPattern {
                rate_bpm: 16.0,
                amplitude: 0.8,
                regularity: 0.9,
                pattern_type: BreathingType::Normal,
            }),
            None,
            MovementProfile::default(),
        )
    }

    fn gross_movement_reading() -> VitalSignsReading {
        VitalSignsReading::new(
            None,
            None,
            MovementProfile {
                movement_type: MovementType::Gross,
                intensity: 0.8,
                frequency: 1.2,
                is_voluntary: true,
            },
        )
    }

    #[test]
    fn breathing_presence_is_stationary_and_confirmed() {
        let v = breathing_reading();
        assert_eq!(PersonContact::classify_motion(&v), Motion::Stationary);
        assert_eq!(PersonContact::classify_life_sign(&v), LifeSign::BreathingConfirmed);
    }

    #[test]
    fn gross_movement_is_moving() {
        let v = gross_movement_reading();
        assert_eq!(PersonContact::classify_motion(&v), Motion::Moving);
        // No clean breathing rhythm but strong movement => movement-only life sign.
        assert_eq!(PersonContact::classify_life_sign(&v), LifeSign::MovementOnly);
    }
}
