//! `ruview-tactical` — serve the tactical dashboard + JSON/WebSocket API.
//!
//! By default it loads the built-in simulated hostage scenario so the app is
//! demonstrable with no hardware. Point a real CSI pipeline at `POST /api/reading`
//! (and `POST /api/structure` with your floor plan) to drive it live; pass
//! `--no-sim` to start empty and wait for real readings.

use wifi_densepose_tactical::api::{router, spawn_sim_loop, AppState};
use wifi_densepose_tactical::engine::TacticalEngine;
use wifi_densepose_tactical::sim::demo_structure;

#[tokio::main]
async fn main() {
    tracing_subscriber_init();

    let args: Vec<String> = std::env::args().collect();
    let sim_enabled = !args.iter().any(|a| a == "--no-sim");
    let bind = std::env::var("TACTICAL_BIND").unwrap_or_else(|_| "127.0.0.1:8099".to_string());

    // Load the demo structure so the dashboard renders a floor plan immediately.
    // In `--no-sim` mode it still loads the layout but stays unoccupied until a
    // real pipeline posts readings.
    let engine = TacticalEngine::new(demo_structure());
    let state = AppState::new(engine);

    if sim_enabled {
        spawn_sim_loop(state.clone());
        eprintln!("[ruview-tactical] SIMULATION MODE — synthetic contacts, no live radio.");
    } else {
        eprintln!("[ruview-tactical] live mode — waiting for POST /api/reading.");
    }

    eprintln!("[ruview-tactical] dashboard:  http://{bind}/");
    eprintln!("[ruview-tactical] ⚠ decision-support only — see the banner in the UI.");

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[ruview-tactical] failed to bind {bind}: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = axum::serve(listener, router(state)).await {
        eprintln!("[ruview-tactical] server error: {e}");
        std::process::exit(1);
    }
}

/// Minimal tracing init without pulling `tracing-subscriber` as a hard dep:
/// this binary only needs stderr breadcrumbs, which `eprintln!` covers. Kept as
/// a named no-op so the call site reads intentionally.
fn tracing_subscriber_init() {}
