//! # WiFi-DensePose Tactical
//!
//! A **decision-support** layer that turns RuView's WiFi Channel-State-Information
//! (CSI) through-wall sensing into a live tactical picture: where human presence is
//! detected inside a structure, room by room, whether those contacts are moving or
//! stationary, and whether breathing is detected (a life sign). It is designed for
//! law-enforcement and rescue teams planning entry into a room or building — for
//! example a hostage-rescue element that needs to know how many people are inside
//! and roughly where, before making entry.
//!
//! It is built on [`wifi_densepose_mat`] (the Mass Casualty Assessment Tool), reusing
//! its validated vital-signs detection, RSSI triangulation, and 3D-coordinate model,
//! and re-frames those primitives around a *building* rather than a debris field.
//!
//! ## ⚠️ Operational safety — READ THIS
//!
//! This is a **planning aid, not ground truth**, and it has hard physical limits.
//! WiFi sensing:
//!
//! - **CANNOT distinguish a hostage from a hostage-taker.** A [`PersonContact`] is a
//!   detected *human presence only*. It carries no identity, no intent, and no
//!   friend/foe label — deliberately. Do not treat any contact as a threat or as a
//!   protected person.
//! - **CANNOT confirm a weapon**, count with certainty, or tell you a person's exact
//!   posture or pose.
//! - **Absence of a contact does NOT mean a room is empty.** A perfectly still person,
//!   heavy construction, metal, or poor sensor geometry can all hide a real occupant.
//! - Produces **coarse** positions. With fewer than three sensors around a room the
//!   system reports *room-level* presence only, not a precise point.
//! - Has **no validated accuracy** for tactical use in this repository. Numbers here
//!   are engineering estimates and, in [`sim`] mode, are wholly synthetic.
//!
//! Every output is advisory. It must be corroborated with other intelligence and must
//! never be the sole basis for a use-of-force decision. The bundled
//! [`entry`] advisor exists to *organize* what the sensors saw, not to authorize entry.
//!
//! ## Architecture
//!
//! ```text
//!   RoomReading (CSI-derived vitals + sensor RSSI, or simulated)
//!        │
//!        ▼
//!   TacticalEngine ──uses──▶ wifi-densepose-mat (detection, localization)
//!        │  tracks per-room PersonContacts, smooths, prunes stale
//!        ▼
//!   TacticalPicture ──▶ EntryAdvisor ──▶ EntryRecommendation[]
//!        │
//!        ▼
//!   api (Axum) ──serves──▶ tactical dashboard (top-down floor plan, live)
//! ```

// The whole crate is safe Rust, except the single `#[no_mangle]` JNI export in
// `android.rs` (which the `unsafe_code` lint classes as unsafe). Keep the forbid
// for every other build; relax to `deny` under the `android` feature so that one
// export can opt out locally.
#![cfg_attr(not(feature = "android"), forbid(unsafe_code))]
#![cfg_attr(feature = "android", deny(unsafe_code))]
#![warn(missing_docs)]

pub mod domain;
pub mod engine;
pub mod entry;
mod error;

#[cfg(feature = "sim")]
pub mod sim;

#[cfg(feature = "api")]
pub mod api;

/// JNI entry point for the Android APK shell. See `android/` for the build.
#[cfg(feature = "android")]
pub mod android;

pub use domain::{
    contact::{ContactId, LifeSign, Motion, PersonContact},
    picture::{RoomOccupancy, TacticalPicture},
    reading::{MovementLevel, ReadingInput, RoomReading, SensorRssi},
    structure::{Room, RoomId, Structure, StructureId},
};
pub use engine::{EngineConfig, TacticalEngine};
pub use entry::{EntryAdvisor, EntryRecommendation, RoomAssessment, TacticalPriority};
pub use error::TacticalError;

/// Library version (from `CARGO_PKG_VERSION`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Convenience result type for this crate.
pub type Result<T> = std::result::Result<T, TacticalError>;
