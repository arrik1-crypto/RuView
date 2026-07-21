//! Tactical domain model.
//!
//! Bounded context for tactical entry planning, distinct from the disaster /
//! mass-casualty context in [`wifi_densepose_mat`]. The nouns here are a
//! *building* the way an entry team sees it: a [`structure::Structure`] made of
//! [`structure::Room`]s, live [`contact::PersonContact`]s inside them, and the
//! [`picture::TacticalPicture`] that aggregates them for the operator.

pub mod contact;
pub mod picture;
pub mod reading;
pub mod structure;
