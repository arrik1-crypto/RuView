//! JNI entry point for the Android APK shell (feature = "android").
//!
//! The Kotlin side does `System.loadLibrary("wifi_densepose_tactical")` and calls
//! `TacticalServer.nativeStart(port, demo)`, which spawns the Axum server on a
//! background thread bound to `0.0.0.0:<port>` — so an ESP32/CSI sensor mesh on
//! the LAN can `POST /api/reading` to the phone — and the app points a WebView at
//! `http://127.0.0.1:<port>/`.
//!
//! Binding `0.0.0.0` means the phone is the aggregation target: provision your
//! sensor nodes with the phone's LAN IP (the RuView firmware's `--target-ip`).
//! The dashboard itself is loaded over loopback and needs no network.

use std::sync::atomic::{AtomicBool, Ordering};

use jni::objects::JClass;
use jni::sys::{jboolean, jint};
use jni::JNIEnv;

use crate::api::{router, spawn_sim_loop, AppState};
use crate::engine::TacticalEngine;
use crate::sim::demo_structure;

/// Guards against a second `nativeStart` (e.g. on Android activity recreation)
/// spawning a duplicate runtime that would fail to bind the same port.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Start the tactical server. Non-blocking: returns immediately after spawning
/// the runtime thread so the Android main thread stays responsive.
///
/// - `port`: TCP port to bind (`<= 0` → 8099).
/// - `demo`: when non-zero, runs the built-in synthetic scenario; when zero,
///   starts in live mode and waits for real `POST /api/reading` traffic.
///
/// # Safety
/// Exported for JNI; must only be invoked by the JVM with the matching
/// `TacticalServer.nativeStart` declaration.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "system" fn Java_com_ruvnet_tactical_TacticalServer_nativeStart(
    _env: JNIEnv,
    _class: JClass,
    port: jint,
    demo: jboolean,
) {
    // Idempotent: a second call (activity recreation, config change) is a no-op
    // rather than a second runtime racing to bind the same port.
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let port: u16 = if port <= 0 { 8099 } else { port as u16 };
    let demo = demo != 0;

    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("[tactical-jni] failed to build runtime: {e}");
                return;
            }
        };

        runtime.block_on(async move {
            // Load the demo floor plan so the dashboard renders immediately; a
            // real deployment replaces it via `POST /api/structure`.
            let engine = TacticalEngine::new(demo_structure());
            let state = AppState::new(engine);

            if demo {
                spawn_sim_loop(state.clone());
            }

            let bind = format!("0.0.0.0:{port}");
            match tokio::net::TcpListener::bind(&bind).await {
                Ok(listener) => {
                    if let Err(e) = axum::serve(listener, router(state)).await {
                        eprintln!("[tactical-jni] server error: {e}");
                    }
                }
                Err(e) => eprintln!("[tactical-jni] failed to bind {bind}: {e}"),
            }
        });
    });
}
