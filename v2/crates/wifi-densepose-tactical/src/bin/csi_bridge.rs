//! `ruview-csi-bridge` — ESP32 UDP CSI → tactical `/api/csi`.
//!
//! Closes the seam between the RuView sensor mesh and the tactical app. ESP32/CSI
//! nodes stream ADR-018 binary CSI frames over UDP (magic `0xC5110001`); this tool
//! listens for them, decodes each frame with `wifi-densepose-hardware`, reduces it
//! to a scalar amplitude/phase sample, buffers per room, and forwards a
//! time-series chunk to a tactical server's `POST /api/csi` every flush interval.
//!
//! It is a **host-side aggregator** — run it on a laptop / Raspberry Pi / the
//! phone's companion, on the same LAN as both the nodes and the tactical server.
//! It is not part of the APK or the library (feature = "bridge").
//!
//! Usage: `ruview-csi-bridge [config.json]` (default `csi-bridge.json`).
//! See `examples/csi-bridge.example.json` for the config shape.
//!
//! Honest scope: one primary node per room drives the breathing/movement
//! time-series (amplitude proxy); every node in the room contributes its latest
//! RSSI for localization. This is presence + coarse location, not validated
//! accuracy — see the crate root safety notes.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use wifi_densepose_hardware::{CsiFrame, Esp32CsiParser};

/// Hard cap on buffered samples per node (≈ minutes at CSI rates) so a
/// disconnected server can't grow the buffer without bound.
const MAX_SAMPLES: usize = 4000;

fn default_server() -> String {
    "http://127.0.0.1:8099".to_string()
}
fn default_listen() -> String {
    "0.0.0.0:5566".to_string()
}
fn default_flush_ms() -> u64 {
    1000
}

/// One node's placement, from the config file.
#[derive(Debug, Clone, Deserialize)]
struct NodeCfg {
    node_id: u8,
    room: String,
    sensor_id: String,
    #[serde(default)]
    primary: bool,
}

/// Bridge configuration (JSON).
#[derive(Debug, Clone, Deserialize)]
struct Config {
    #[serde(default = "default_server")]
    server: String,
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_flush_ms")]
    flush_ms: u64,
    nodes: Vec<NodeCfg>,
}

/// Live per-node accumulation. (Room membership lives in `room_nodes`.)
struct NodeState {
    sensor_id: String,
    is_primary: bool,
    amps: Vec<f64>,
    phases: Vec<f64>,
    last_rssi: Option<f64>,
}

/// A forward-ready payload for one room.
#[derive(Debug, PartialEq)]
struct CsiPost {
    room: String,
    amplitudes: Vec<f64>,
    phases: Vec<f64>,
    sensor_rssi: Vec<(String, f64)>,
}

/// Bridge state: node buffers + room→node index.
struct BridgeState {
    nodes: HashMap<u8, NodeState>,
    /// Rooms in first-seen order (stable output).
    rooms: Vec<String>,
    room_nodes: HashMap<String, Vec<u8>>,
}

impl BridgeState {
    fn from_config(cfg: &Config) -> Self {
        let mut nodes = HashMap::new();
        let mut rooms: Vec<String> = Vec::new();
        let mut room_nodes: HashMap<String, Vec<u8>> = HashMap::new();

        for n in &cfg.nodes {
            if !rooms.contains(&n.room) {
                rooms.push(n.room.clone());
            }
            let entry = room_nodes.entry(n.room.clone()).or_default();
            entry.push(n.node_id);
            nodes.insert(
                n.node_id,
                NodeState {
                    sensor_id: n.sensor_id.clone(),
                    is_primary: n.primary,
                    amps: Vec::new(),
                    phases: Vec::new(),
                    last_rssi: None,
                },
            );
        }

        // Ensure each room has exactly one primary: if none was flagged, the
        // first node listed for the room becomes primary.
        for ids in room_nodes.values() {
            let has_primary = ids.iter().any(|id| nodes[id].is_primary);
            if !has_primary {
                if let Some(first) = ids.first() {
                    if let Some(ns) = nodes.get_mut(first) {
                        ns.is_primary = true;
                    }
                }
            }
        }

        Self {
            nodes,
            rooms,
            room_nodes,
        }
    }

    /// Decode a UDP datagram and fold its CSI frames into the buffers.
    /// Returns the number of mapped frames ingested.
    fn ingest_datagram(&mut self, buf: &[u8]) -> usize {
        let (frames, _consumed) = Esp32CsiParser::parse_stream(buf);
        let mut ingested = 0;
        for frame in frames {
            let node_id = frame.metadata.node_id;
            let Some(ns) = self.nodes.get_mut(&node_id) else {
                continue; // node not in the floor-plan config — skip
            };
            ns.last_rssi = Some(frame.metadata.rssi_dbm as f64);
            if ns.is_primary {
                let (amp, phase) = reduce_frame(&frame);
                ns.amps.push(amp);
                ns.phases.push(phase);
                if ns.amps.len() > MAX_SAMPLES {
                    let drop = ns.amps.len() - MAX_SAMPLES;
                    ns.amps.drain(0..drop);
                    ns.phases.drain(0..drop);
                }
            }
            ingested += 1;
        }
        ingested
    }

    /// Drain buffered samples into one `CsiPost` per room that has fresh primary
    /// data. Clears the primary amplitude/phase buffers; keeps last RSSI.
    fn drain_posts(&mut self) -> Vec<CsiPost> {
        let mut posts = Vec::new();
        // Snapshot the room ordering to avoid borrow conflicts.
        let rooms = self.rooms.clone();
        for room in rooms {
            let ids = match self.room_nodes.get(&room) {
                Some(v) => v.clone(),
                None => continue,
            };

            // Collect RSSI from every node in the room.
            let mut sensor_rssi = Vec::new();
            for id in &ids {
                if let Some(ns) = self.nodes.get(id) {
                    if let Some(r) = ns.last_rssi {
                        sensor_rssi.push((ns.sensor_id.clone(), r));
                    }
                }
            }

            // Take the primary node's buffered series.
            let primary_id = ids.iter().find(|id| self.nodes[id].is_primary).copied();
            let (amplitudes, phases) = match primary_id {
                Some(id) => {
                    let ns = self.nodes.get_mut(&id).unwrap();
                    (std::mem::take(&mut ns.amps), std::mem::take(&mut ns.phases))
                }
                None => (Vec::new(), Vec::new()),
            };

            if amplitudes.is_empty() {
                continue; // nothing new for this room this interval
            }

            posts.push(CsiPost {
                room,
                amplitudes,
                phases,
                sensor_rssi,
            });
        }
        posts
    }
}

/// Reduce a per-subcarrier CSI frame to one `(amplitude, phase)` time sample:
/// mean amplitude across subcarriers (a standard breathing proxy) and the phase
/// of the strongest subcarrier (most reliable phase).
fn reduce_frame(frame: &CsiFrame) -> (f64, f64) {
    let amp = frame.mean_amplitude();
    let (amps, phases) = frame.to_amplitude_phase();
    let phase = amps
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(idx, _)| phases[idx])
        .unwrap_or(0.0);
    (amp, phase)
}

fn post_to_server(agent: &ureq::Agent, url: &str, post: &CsiPost) {
    let body = serde_json::json!({
        "room_name": post.room,
        "amplitudes": post.amplitudes,
        "phases": post.phases,
        "sensor_rssi": post.sensor_rssi
            .iter()
            .map(|(id, r)| serde_json::json!({ "id": id, "rssi": r }))
            .collect::<Vec<_>>(),
    });
    match agent.post(url).send_json(body) {
        Ok(_) => {}
        Err(e) => eprintln!("[csi-bridge] POST {url} failed: {e}"),
    }
}

fn main() {
    let config_path = std::env::args().nth(1).unwrap_or_else(|| "csi-bridge.json".to_string());
    if config_path == "--help" || config_path == "-h" {
        eprintln!("usage: ruview-csi-bridge [config.json]");
        eprintln!("  listens for ESP32 ADR-018 UDP CSI and forwards to a tactical /api/csi.");
        eprintln!("  see examples/csi-bridge.example.json for the config shape.");
        return;
    }

    let cfg: Config = match std::fs::read_to_string(&config_path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[csi-bridge] failed to load config {config_path}: {e}");
            std::process::exit(1);
        }
    };

    let csi_url = format!("{}/api/csi", cfg.server.trim_end_matches('/'));
    let state = Arc::new(Mutex::new(BridgeState::from_config(&cfg)));

    let socket = match UdpSocket::bind(&cfg.listen) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[csi-bridge] failed to bind {}: {e}", cfg.listen);
            std::process::exit(1);
        }
    };

    let room_count = { state.lock().unwrap().rooms.len() };
    eprintln!("[csi-bridge] listening for ESP32 CSI on udp://{}", cfg.listen);
    eprintln!("[csi-bridge] forwarding to {csi_url}");
    eprintln!(
        "[csi-bridge] {} node(s) across {room_count} room(s), flush every {} ms",
        cfg.nodes.len(),
        cfg.flush_ms
    );
    eprintln!("[csi-bridge] ⚠ presence + coarse location only — decision-support, not ground truth.");

    // Receiver thread: decode datagrams into the shared buffers.
    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((n, _peer)) => {
                        state.lock().unwrap().ingest_datagram(&buf[..n]);
                    }
                    Err(e) => {
                        eprintln!("[csi-bridge] recv error: {e}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        });
    }

    // Flush loop: forward buffered series to the tactical server.
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .build();
    let flush = Duration::from_millis(cfg.flush_ms.max(100));
    loop {
        std::thread::sleep(flush);
        let posts = { state.lock().unwrap().drain_posts() };
        for post in &posts {
            post_to_server(&agent, &csi_url, post);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal ADR-018 frame: 20-byte header + `pairs` I/Q bytes.
    fn frame_bytes(node_id: u8, rssi: i8, pairs: &[(i8, i8)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0xC511_0001u32.to_le_bytes()); // magic
        buf.push(node_id); // node id
        buf.push(1); // n antennas
        buf.extend_from_slice(&(pairs.len() as u16).to_le_bytes()); // n subcarriers
        buf.extend_from_slice(&2437u32.to_le_bytes()); // freq MHz
        buf.extend_from_slice(&1u32.to_le_bytes()); // sequence
        buf.push(rssi as u8); // rssi
        buf.push((-95i8) as u8); // noise
        buf.push(0); // ppdu
        buf.push(0); // flags
        for (i, q) in pairs {
            buf.push(*i as u8);
            buf.push(*q as u8);
        }
        buf
    }

    fn config() -> Config {
        Config {
            server: default_server(),
            listen: default_listen(),
            flush_ms: 1000,
            nodes: vec![
                NodeCfg { node_id: 1, room: "Den".into(), sensor_id: "den-a".into(), primary: true },
                NodeCfg { node_id: 2, room: "Den".into(), sensor_id: "den-b".into(), primary: false },
            ],
        }
    }

    #[test]
    fn config_maps_rooms_and_primary() {
        let state = BridgeState::from_config(&config());
        assert_eq!(state.rooms, vec!["Den"]);
        assert_eq!(state.room_nodes["Den"], vec![1, 2]);
        assert!(state.nodes[&1].is_primary);
        assert!(!state.nodes[&2].is_primary);
    }

    #[test]
    fn defaulted_primary_is_first_node() {
        let mut cfg = config();
        cfg.nodes[0].primary = false; // no explicit primary
        let state = BridgeState::from_config(&cfg);
        assert!(state.nodes[&1].is_primary, "first listed node defaults to primary");
        assert!(!state.nodes[&2].is_primary);
    }

    #[test]
    fn ingest_buffers_primary_series_and_all_rssi() {
        let mut state = BridgeState::from_config(&config());
        // Two frames from the primary node, one from the secondary.
        state.ingest_datagram(&frame_bytes(1, -55, &[(100, 0), (0, 50)]));
        state.ingest_datagram(&frame_bytes(1, -54, &[(100, 0), (0, 50)]));
        state.ingest_datagram(&frame_bytes(2, -61, &[(30, 40)]));

        assert_eq!(state.nodes[&1].amps.len(), 2, "primary buffers a sample per frame");
        assert!(state.nodes[&2].amps.is_empty(), "secondary buffers no series");
        assert_eq!(state.nodes[&2].last_rssi, Some(-61.0), "secondary still reports RSSI");
    }

    #[test]
    fn drain_produces_one_post_with_series_and_rssi() {
        let mut state = BridgeState::from_config(&config());
        state.ingest_datagram(&frame_bytes(1, -55, &[(100, 0), (0, 50)]));
        state.ingest_datagram(&frame_bytes(2, -61, &[(30, 40)]));

        let posts = state.drain_posts();
        assert_eq!(posts.len(), 1);
        let p = &posts[0];
        assert_eq!(p.room, "Den");
        assert_eq!(p.amplitudes.len(), 1);
        assert_eq!(p.phases.len(), 1);
        // Both nodes' RSSI present for localization.
        assert!(p.sensor_rssi.iter().any(|(id, _)| id == "den-a"));
        assert!(p.sensor_rssi.iter().any(|(id, _)| id == "den-b"));
        // Buffers cleared after draining.
        assert!(state.nodes[&1].amps.is_empty());
        // A second drain with no new data yields nothing.
        assert!(state.drain_posts().is_empty());
    }

    #[test]
    fn unmapped_node_is_skipped() {
        let mut state = BridgeState::from_config(&config());
        let n = state.ingest_datagram(&frame_bytes(99, -50, &[(10, 10)]));
        assert_eq!(n, 0, "frames from unknown nodes are ignored");
    }

    #[test]
    fn reduce_frame_mean_amplitude() {
        let (frames, _) = Esp32CsiParser::parse_stream(&frame_bytes(1, -50, &[(100, 0), (0, 50)]));
        let (amp, _phase) = reduce_frame(&frames[0]);
        // mean(|100|, |50|) = 75
        assert!((amp - 75.0).abs() < 0.01);
    }
}
