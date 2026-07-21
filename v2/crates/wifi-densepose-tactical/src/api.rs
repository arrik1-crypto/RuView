//! Axum web surface: JSON API + WebSocket + the bundled tactical dashboard.
//!
//! Requires the `api` feature. State is a shared [`TacticalEngine`] behind an
//! async `RwLock`, plus a broadcast channel that pushes each new
//! [`TacticalPicture`](crate::domain::picture::TacticalPicture) to connected
//! dashboards.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::CorsLayer;

use crate::ble::{BleObservation, BleTracker};
use crate::domain::picture::TacticalPicture;
use crate::domain::reading::{ReadingInput, RoomReading, SensorRssi};
use crate::domain::structure::{RoomId, Structure};
use crate::engine::TacticalEngine;
use crate::entry::EntryAdvisor;
use crate::ingest::{CsiIngest, DEFAULT_CSI_SAMPLE_RATE};

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// The tactical engine, guarded for concurrent access.
    pub engine: Arc<RwLock<TacticalEngine>>,
    /// Broadcast of serialized [`TacticalPicture`] JSON to live dashboards.
    pub updates: broadcast::Sender<String>,
    /// Raw-CSI ingest bridge (one MAT detection pipeline per room).
    pub ingest: Arc<RwLock<CsiIngest>>,
    /// Auxiliary BLE device-presence tracker (phone-native).
    pub ble: Arc<RwLock<BleTracker>>,
}

impl AppState {
    /// Build state around an engine.
    pub fn new(engine: TacticalEngine) -> Self {
        let (updates, _) = broadcast::channel(64);
        let ingest = CsiIngest::from_structure(engine.structure(), DEFAULT_CSI_SAMPLE_RATE);
        Self {
            engine: Arc::new(RwLock::new(engine)),
            updates,
            ingest: Arc::new(RwLock::new(ingest)),
            ble: Arc::new(RwLock::new(BleTracker::new())),
        }
    }

    /// The full current picture: the engine's tactical picture with the BLE
    /// device layer attached. Every client-facing response goes through this so
    /// the two layers stay in one payload.
    pub async fn current_picture(&self) -> TacticalPicture {
        let mut picture = { self.engine.read().await.picture() };
        picture.ble_devices = { self.ble.read().await.snapshot() };
        picture
    }

    /// Serialize the current picture and push it to all subscribers.
    pub async fn broadcast_picture(&self) {
        let picture = self.current_picture().await;
        if let Ok(json) = serde_json::to_string(&picture) {
            // Ignore send errors: they just mean no dashboards are connected.
            let _ = self.updates.send(json);
        }
    }
}

/// Build the router with all routes and permissive CORS (LAN tactical use).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/structure", get(get_structure).post(post_structure))
        .route("/api/reading", post(post_reading))
        .route("/api/csi", post(post_csi))
        .route("/api/picture", get(get_picture))
        .route("/api/entry", get(get_entry))
        .route("/api/sensors", get(get_sensors))
        .route("/api/ble", get(get_ble).post(post_ble))
        .route("/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// The bundled single-file dashboard.
async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/tactical.html"))
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "wifi-densepose-tactical",
        "version": crate::VERSION,
        "advisory": crate::domain::picture::STANDING_ADVISORY,
    }))
}

async fn get_structure(State(state): State<AppState>) -> impl IntoResponse {
    let structure = { state.engine.read().await.structure().clone() };
    Json(structure)
}

async fn post_structure(
    State(state): State<AppState>,
    Json(structure): Json<Structure>,
) -> impl IntoResponse {
    {
        let mut engine = state.engine.write().await;
        engine.set_structure(structure);
    }
    // Rebuild the CSI pipelines so they match the new room set.
    {
        let structure = state.engine.read().await.structure().clone();
        state.ingest.write().await.rebuild(&structure);
    }
    state.broadcast_picture().await;
    StatusCode::NO_CONTENT
}

/// A raw CSI frame for one room, as pushed by a sensor node.
#[derive(serde::Deserialize)]
struct CsiFrameInput {
    #[serde(default)]
    room_id: Option<RoomId>,
    #[serde(default)]
    room_name: Option<String>,
    /// CSI amplitude samples.
    amplitudes: Vec<f64>,
    /// Unwrapped CSI phase samples (same length as `amplitudes`).
    phases: Vec<f64>,
    /// Per-sensor RSSI for localization (optional).
    #[serde(default)]
    sensor_rssi: Vec<SensorRssi>,
}

/// Ingest raw CSI: buffer it in the room's MAT pipeline, distil vitals, and — if
/// a detection lands — feed the engine (else record an absence). Returns the
/// updated picture so a node can see the effect of its frame.
async fn post_csi(
    State(state): State<AppState>,
    Json(frame): Json<CsiFrameInput>,
) -> impl IntoResponse {
    let room_id = {
        let engine = state.engine.read().await;
        engine.resolve_room(frame.room_id, frame.room_name.as_deref())
    };
    let Some(room_id) = room_id else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "unknown room" })),
        )
            .into_response();
    };

    let vitals = {
        let ingest = state.ingest.read().await;
        if !ingest.push(room_id, &frame.amplitudes, &frame.phases) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "unknown room" })),
            )
                .into_response();
        }
        match ingest.distill(room_id).await {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response()
            }
        }
    };

    let sensor_rssi: Vec<(String, f64)> = frame
        .sensor_rssi
        .iter()
        .map(|s| (s.id.clone(), s.rssi))
        .collect();
    {
        let mut engine = state.engine.write().await;
        // Record node activity regardless of the detection outcome, so a node
        // reporting "no one here" still counts as reporting.
        engine.note_sensors(room_id, &sensor_rssi);
        match vitals {
            Some(v) if v.has_vitals() => {
                let reading = RoomReading {
                    room_id,
                    vitals: v,
                    occupancy: 1,
                    sensor_rssi,
                };
                let _ = engine.ingest(reading);
            }
            // Enough data but nothing detected → the room reads clear.
            Some(_) => {
                let _ = engine.record_absence(room_id);
            }
            // Not enough buffered CSI yet → leave the current picture untouched.
            None => {}
        }
    }

    state.broadcast_picture().await;
    (StatusCode::OK, Json(state.current_picture().await)).into_response()
}

async fn post_reading(
    State(state): State<AppState>,
    Json(input): Json<ReadingInput>,
) -> impl IntoResponse {
    let result = {
        let mut engine = state.engine.write().await;
        engine.apply_input(&input)
    };
    match result {
        Ok(()) => {
            state.broadcast_picture().await;
            (StatusCode::OK, Json(state.current_picture().await)).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn get_picture(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.current_picture().await)
}

async fn get_entry(State(state): State<AppState>) -> impl IntoResponse {
    let picture = { state.engine.read().await.picture() };
    Json(EntryAdvisor::assess(&picture))
}

async fn get_sensors(State(state): State<AppState>) -> impl IntoResponse {
    let report = { state.engine.read().await.sensor_report() };
    Json(report)
}

async fn get_ble(State(state): State<AppState>) -> impl IntoResponse {
    let devices = { state.ble.read().await.snapshot() };
    Json(devices)
}

/// A batch of BLE observations from the phone scanner.
#[derive(serde::Deserialize)]
struct BleScanInput {
    devices: Vec<BleObservation>,
}

/// Ingest a BLE scan batch: update the tracker, prune stale devices, broadcast.
async fn post_ble(
    State(state): State<AppState>,
    Json(scan): Json<BleScanInput>,
) -> impl IntoResponse {
    {
        let mut ble = state.ble.write().await;
        ble.observe(&scan.devices);
        ble.prune();
    }
    state.broadcast_picture().await;
    let devices = { state.ble.read().await.snapshot() };
    (StatusCode::OK, Json(devices)).into_response()
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    // Send the current picture immediately so a new dashboard is not blank.
    let mut rx = state.updates.subscribe();
    let current = state.current_picture().await;
    if let Ok(json) = serde_json::to_string(&current) {
        if socket.send(Message::Text(json)).await.is_err() {
            return;
        }
    }

    // Then stream updates until the client disconnects.
    loop {
        match rx.recv().await {
            Ok(json) => {
                if socket.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
            // Lagged: skip missed frames, keep going (next picture is a full snapshot).
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Drive the built-in scenario in the background: tick, apply readings, prune
/// stale contacts, broadcast the new picture — roughly once per sensing cycle.
/// Shared by the desktop binary and the Android JNI entry point. Overwrites the
/// engine's structure with the scenario's so room ids line up with its readings.
pub fn spawn_sim_loop(state: AppState) {
    tokio::spawn(async move {
        let mut scenario = crate::sim::Scenario::new();
        {
            let mut engine = state.engine.write().await;
            engine.set_structure(scenario.structure().clone());
        }
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(700));
        let mut k: u32 = 0;
        loop {
            interval.tick().await;
            k = k.wrapping_add(1);
            let readings = scenario.tick();
            {
                let mut engine = state.engine.write().await;
                for input in &readings {
                    let _ = engine.apply_input(input);
                }
                engine.prune_stale();
            }
            // Synthetic BLE devices so the "devices (not people)" overlay is
            // exercised in demo mode. Purely illustrative — not real radios.
            {
                let wobble = (k % 20) as f64 - 10.0;
                let mut ble = state.ble.write().await;
                ble.observe(&[
                    crate::ble::BleObservation {
                        id: "7a:11:22:demo-01".into(),
                        name: Some("Phone".into()),
                        rssi: -62.0 + wobble * 0.5,
                    },
                    crate::ble::BleObservation {
                        id: "c3:44:55:demo-02".into(),
                        name: None,
                        rssi: -79.0 + wobble * 0.3,
                    },
                ]);
                ble.prune();
            }
            state.broadcast_picture().await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::demo_structure;

    #[tokio::test]
    async fn broadcast_reaches_subscriber() {
        let state = AppState::new(TacticalEngine::new(demo_structure()));
        let mut rx = state.updates.subscribe();
        state.broadcast_picture().await;
        let json = rx.try_recv().expect("a picture should have been broadcast");
        assert!(json.contains("advisory"));
    }

    #[tokio::test]
    async fn apply_input_through_state() {
        let state = AppState::new(TacticalEngine::new(demo_structure()));
        let input = ReadingInput {
            room_id: None,
            room_name: Some("Bedroom 2".into()),
            presence: true,
            breathing_bpm: Some(16.0),
            movement: crate::domain::reading::MovementLevel::None,
            occupancy: Some(2),
            sensor_rssi: vec![],
        };
        {
            let mut engine = state.engine.write().await;
            engine.apply_input(&input).unwrap();
        }
        let picture = state.engine.read().await.picture();
        assert_eq!(picture.total_occupancy_estimate, 2);
        assert_eq!(picture.occupied_rooms, 1);
    }
}
