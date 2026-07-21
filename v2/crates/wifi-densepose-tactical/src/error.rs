//! Error type for tactical operations.

/// Errors produced by the tactical engine and its API.
#[derive(Debug, thiserror::Error)]
pub enum TacticalError {
    /// A reading referenced a room that is not part of the loaded structure.
    #[error("unknown room: {0}")]
    UnknownRoom(String),

    /// The structure has no rooms, so no picture can be produced.
    #[error("structure has no rooms defined")]
    EmptyStructure,

    /// A supplied value was outside its valid range.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// Serialization / deserialization failure at the API boundary.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
