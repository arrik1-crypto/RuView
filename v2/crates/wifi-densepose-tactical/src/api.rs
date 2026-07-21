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

use crate::domain::reading::ReadingInput;
use crate::domain::structure::Structure;
use crate::engine::TacticalEngine;
use crate::entry::EntryAdvisor;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// The tactical engine, guarded for concurrent access.
    pub engine: Arc<RwLock<TacticalEngine>>,
    /// Broadcast of serialized [`TacticalPicture`] JSON to live dashboards.
    pub updates: broadcast::Sender<String>,
}

impl AppState {
    /// Build state around an engine.
    pub fn new(engine: TacticalEngine) -> Self {
        let (updates, _) = broadcast::channel(64);
        Self {
            engine: Arc::new(RwLock::new(engine)),
            updates,
        }
    }

    /// Serialize the current picture and push it to all subscribers.
    pub async fn broadcast_picture(&self) {
        let picture = { self.engine.read().await.picture() };
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
        .route("/api/picture", get(get_picture))
        .route("/api/entry", get(get_entry))
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
    state.broadcast_picture().await;
    StatusCode::NO_CONTENT
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
            let picture = { state.engine.read().await.picture() };
            (StatusCode::OK, Json(picture)).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn get_picture(State(state): State<AppState>) -> impl IntoResponse {
    let picture = { state.engine.read().await.picture() };
    Json(picture)
}

async fn get_entry(State(state): State<AppState>) -> impl IntoResponse {
    let picture = { state.engine.read().await.picture() };
    Json(EntryAdvisor::assess(&picture))
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
    let current = { state.engine.read().await.picture() };
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
