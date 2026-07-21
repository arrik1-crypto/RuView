//! Raw-CSI ingest bridge: ESP32/CSI frames → MAT detection → `RoomReading`.
//!
//! The tactical engine consumes *distilled* readings (presence, breathing,
//! movement). A sensor mesh, though, streams raw Channel-State-Information. This
//! bridge closes that seam on-device: it holds a MAT [`DetectionPipeline`] per
//! room, accumulates the amplitude/phase frames a node pushes, and once enough
//! signal is buffered runs MAT's breathing/movement detectors to produce a
//! [`VitalSignsReading`]. The API layer turns that into a `RoomReading` (adding
//! the frame's per-sensor RSSI for localization) and feeds the engine.
//!
//! Requires the `api` feature (async detection + tokio). No ONNX: MAT's ML
//! enhancement stays off, so detection is the pure signal-processing path.

use std::collections::HashMap;
use std::sync::Arc;

use wifi_densepose_mat::domain::VitalSignsReading;
use wifi_densepose_mat::{DetectionConfig, DetectionPipeline};

use crate::domain::structure::{RoomId, Structure};
use crate::error::TacticalError;

/// Default CSI sample rate (Hz). ESP32-class nodes typically deliver ~100 CSI
/// frames/sec; MAT needs ~5 s of data before it will report a detection.
pub const DEFAULT_CSI_SAMPLE_RATE: f64 = 100.0;

/// Per-room CSI accumulation + detection. Held behind an `Arc` so a caller can
/// clone the handle out from under the ingest map lock and run the (async)
/// detection without blocking structure rebuilds or other rooms' pushes.
pub struct RoomIngest {
    pipeline: DetectionPipeline,
    zone: wifi_densepose_mat::domain::ScanZone,
}

impl RoomIngest {
    /// Buffer a CSI chunk. Rejects (returns `false`, ingesting nothing) when
    /// `amplitudes` and `phases` differ in length — MAT's buffer keeps the two
    /// series in lockstep and a mismatch would later panic its ring-buffer
    /// trim. This is the wire-boundary guard against a malformed `/api/csi` body.
    pub fn push(&self, amplitudes: &[f64], phases: &[f64]) -> bool {
        if amplitudes.len() != phases.len() {
            return false;
        }
        self.pipeline.add_data(amplitudes, phases);
        true
    }

    /// Distil the buffered CSI into a vital-signs reading.
    pub async fn distill(&self) -> Result<Option<VitalSignsReading>, TacticalError> {
        self.pipeline
            .process_zone(&self.zone)
            .await
            .map_err(|e| TacticalError::Invalid(format!("detection failed: {e}")))
    }
}

/// Holds one MAT detection pipeline per room and distils buffered CSI into
/// vital-signs readings.
pub struct CsiIngest {
    sample_rate: f64,
    rooms: HashMap<RoomId, Arc<RoomIngest>>,
}

impl CsiIngest {
    /// Build a bridge for a structure at the given CSI sample rate.
    pub fn from_structure(structure: &Structure, sample_rate: f64) -> Self {
        let mut ingest = Self {
            sample_rate,
            rooms: HashMap::new(),
        };
        ingest.rebuild(structure);
        ingest
    }

    /// Rebuild pipelines for a (possibly new) structure, discarding buffers.
    pub fn rebuild(&mut self, structure: &Structure) {
        self.rooms.clear();
        for room in &structure.rooms {
            let config = DetectionConfig {
                sample_rate: self.sample_rate,
                ..Default::default()
            };
            self.rooms.insert(
                room.id,
                Arc::new(RoomIngest {
                    pipeline: DetectionPipeline::new(config),
                    // Detection ignores geometry; a sensor-less zone is fine here.
                    // Localization uses the frame's RSSI later, in the engine.
                    zone: room.to_scan_zone(&[]),
                }),
            );
        }
    }

    /// Whether a room is known to this bridge.
    pub fn knows_room(&self, room_id: RoomId) -> bool {
        self.rooms.contains_key(&room_id)
    }

    /// Clone out a room's ingest handle. The caller can then push + distill
    /// without holding the ingest map lock across the (async) detection — so a
    /// concurrent structure rebuild or another room's push never stalls behind
    /// one room's DSP.
    pub fn room(&self, room_id: RoomId) -> Option<Arc<RoomIngest>> {
        self.rooms.get(&room_id).cloned()
    }

    /// Push a CSI frame for a room. Returns `false` if the room is unknown or
    /// the amplitude/phase lengths disagree (see [`RoomIngest::push`]).
    pub fn push(&self, room_id: RoomId, amplitudes: &[f64], phases: &[f64]) -> bool {
        match self.rooms.get(&room_id) {
            Some(r) => r.push(amplitudes, phases),
            None => false,
        }
    }

    /// Distil the buffered CSI for a room into a vital-signs reading. Returns
    /// `Ok(None)` when there is not yet enough data or nothing was detected.
    pub async fn distill(
        &self,
        room_id: RoomId,
    ) -> Result<Option<VitalSignsReading>, TacticalError> {
        let room = self
            .rooms
            .get(&room_id)
            .cloned()
            .ok_or_else(|| TacticalError::UnknownRoom(room_id.to_string()))?;
        room.distill().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::structure::{Room, RoomBounds};

    fn structure() -> (Structure, RoomId) {
        let room = Room::new("Den", 0, RoomBounds::new(0.0, 0.0, 4.0, 4.0));
        let id = room.id;
        (Structure::new("T").with_room(room), id)
    }

    /// Feed ~8 s of synthetic 16-BPM breathing CSI and confirm the bridge
    /// distils a reading with breathing detected.
    #[tokio::test]
    async fn distills_breathing_from_synthetic_csi() {
        let (s, id) = structure();
        let rate = 100.0;
        let ingest = CsiIngest::from_structure(&s, rate);

        let n = (rate * 8.0) as usize;
        let amps: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / rate;
                (2.0 * std::f64::consts::PI * 0.267 * t).sin() // ~16 BPM
            })
            .collect();
        let phases: Vec<f64> = amps.iter().map(|a| a * 0.5).collect();

        assert!(ingest.push(id, &amps, &phases));
        let reading = ingest.distill(id).await.unwrap();
        assert!(reading.is_some(), "8s of breathing CSI should detect vitals");
        assert!(reading.unwrap().has_vitals());
    }

    #[tokio::test]
    async fn insufficient_data_yields_none() {
        let (s, id) = structure();
        let ingest = CsiIngest::from_structure(&s, 100.0);
        // Only 1 s of data — below MAT's 5 s minimum.
        let amps = vec![0.1_f64; 100];
        let phases = vec![0.0_f64; 100];
        ingest.push(id, &amps, &phases);
        assert!(ingest.distill(id).await.unwrap().is_none());
    }

    #[test]
    fn mismatched_amp_phase_lengths_are_rejected() {
        let (s, id) = structure();
        let ingest = CsiIngest::from_structure(&s, 100.0);
        // Would otherwise poison MAT's buffer and panic on the next trim.
        assert!(!ingest.push(id, &vec![0.1; 4000], &[]));
        // A matched push still works.
        assert!(ingest.push(id, &[0.1_f64; 10], &[0.0_f64; 10]));
    }

    #[tokio::test]
    async fn unknown_room_errors() {
        let (s, _) = structure();
        let ingest = CsiIngest::from_structure(&s, 100.0);
        assert!(ingest.distill(RoomId::new()).await.is_err());
    }
}
