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

/// ADR-063 edge fused-vitals packet magic (ESP32-C6 + MR60BHA2 mmWave).
const MMWAVE_MAGIC: u32 = 0xC511_0004;

fn default_server() -> String {
    "http://127.0.0.1:8099".to_string()
}
fn default_listen() -> String {
    // Matches the ESP32 firmware's default UDP target port (CONFIG_CSI_TARGET_PORT
    // = 5005) so nodes and bridge line up with no extra provisioning.
    "0.0.0.0:5005".to_string()
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

/// A forward-ready CSI payload for one room.
#[derive(Debug, PartialEq)]
struct CsiPost {
    room: String,
    amplitudes: Vec<f64>,
    phases: Vec<f64>,
    sensor_rssi: Vec<(String, f64)>,
}

/// A forward-ready mmWave corroboration payload for one room.
#[derive(Debug, Clone, PartialEq)]
struct MmwavePost {
    presence: bool,
    breathing_bpm: Option<f32>,
    heart_rate_bpm: Option<f32>,
    distance_cm: Option<f32>,
    targets: u8,
    confidence: u8,
}

/// Bridge state: node buffers + room→node index.
struct BridgeState {
    nodes: HashMap<u8, NodeState>,
    /// Rooms in first-seen order (stable output).
    rooms: Vec<String>,
    room_nodes: HashMap<String, Vec<u8>>,
    /// Reverse index node_id → room, for routing mmWave packets.
    node_room: HashMap<u8, String>,
    /// Latest mmWave reading pending forward, keyed by room.
    mmwave_pending: HashMap<String, MmwavePost>,
}

impl BridgeState {
    fn from_config(cfg: &Config) -> Self {
        let mut nodes = HashMap::new();
        let mut rooms: Vec<String> = Vec::new();
        let mut room_nodes: HashMap<String, Vec<u8>> = HashMap::new();

        let mut node_room = HashMap::new();
        for n in &cfg.nodes {
            if !rooms.contains(&n.room) {
                rooms.push(n.room.clone());
            }
            let entry = room_nodes.entry(n.room.clone()).or_default();
            entry.push(n.node_id);
            node_room.insert(n.node_id, n.room.clone());
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
            node_room,
            mmwave_pending: HashMap::new(),
        }
    }

    /// Decode a UDP datagram. Routes mmWave fused-vitals packets (0xC5110004) to
    /// the pending mmWave map and everything else through the CSI parser. Returns
    /// the number of mapped items ingested.
    fn ingest_datagram(&mut self, buf: &[u8]) -> usize {
        // mmWave fused-vitals packet? Route it before the CSI parser (which would
        // classify it as a sibling packet and skip it).
        if buf.len() >= 4 {
            let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            if magic == MMWAVE_MAGIC {
                if let Some((node_id, post)) = parse_fused_vitals(buf) {
                    if let Some(room) = self.node_room.get(&node_id) {
                        self.mmwave_pending.insert(room.clone(), post);
                        return 1;
                    }
                }
                return 0;
            }
        }

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

    /// Take the pending mmWave readings (one per room) to forward, clearing them.
    fn drain_mmwave(&mut self) -> Vec<(String, MmwavePost)> {
        std::mem::take(&mut self.mmwave_pending)
            .into_iter()
            .collect()
    }
}

/// Parse an ADR-063 edge fused-vitals packet (magic 0xC5110004, 48 bytes) into
/// its node id and mmWave payload. Byte layout mirrors the firmware's
/// `edge_fused_vitals_pkt_t` (`_Static_assert(sizeof == 48)`). Only the raw
/// mmWave fields are used here — the fused/CSI-side estimates are the tactical
/// engine's job.
fn parse_fused_vitals(buf: &[u8]) -> Option<(u8, MmwavePost)> {
    if buf.len() < 48 {
        return None;
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != MMWAVE_MAGIC {
        return None;
    }
    let node_id = buf[4];
    let flags = buf[5]; // bit0=presence, bit3=mmwave_present
    let n_persons = buf[13];
    let hr = f32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]);
    let br = f32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);
    let dist = f32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]);
    let targets = buf[40];
    let confidence = buf[41];

    let presence =
        (flags & 0x01) != 0 || (flags & 0x08) != 0 || n_persons > 0 || targets > 0;

    Some((
        node_id,
        MmwavePost {
            presence,
            breathing_bpm: (br > 0.0).then_some(br),
            heart_rate_bpm: (hr > 0.0).then_some(hr),
            distance_cm: (dist > 0.0).then_some(dist),
            targets,
            confidence,
        },
    ))
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

fn post_mmwave_to_server(agent: &ureq::Agent, url: &str, room: &str, post: &MmwavePost) {
    let body = serde_json::json!({
        "room_name": room,
        "presence": post.presence,
        "breathing_bpm": post.breathing_bpm,
        "heart_rate_bpm": post.heart_rate_bpm,
        "distance_cm": post.distance_cm,
        "targets": post.targets,
        "confidence": post.confidence,
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

    let base = cfg.server.trim_end_matches('/');
    let csi_url = format!("{base}/api/csi");
    let mmwave_url = format!("{base}/api/mmwave");
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
    eprintln!("[csi-bridge] forwarding CSI to {csi_url}");
    eprintln!("[csi-bridge] forwarding mmWave to {mmwave_url}");
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
        let (posts, mmwave) = {
            let mut s = state.lock().unwrap();
            (s.drain_posts(), s.drain_mmwave())
        };
        for post in &posts {
            post_to_server(&agent, &csi_url, post);
        }
        for (room, post) in &mmwave {
            post_mmwave_to_server(&agent, &mmwave_url, room, post);
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

    /// Build a 48-byte ADR-063 fused-vitals packet with mmWave HR/BR set.
    fn fused_vitals_bytes(node_id: u8, flags: u8, hr: f32, br: f32, dist: f32, targets: u8) -> Vec<u8> {
        let mut buf = vec![0u8; 48];
        buf[0..4].copy_from_slice(&0xC511_0004u32.to_le_bytes());
        buf[4] = node_id;
        buf[5] = flags;
        buf[28..32].copy_from_slice(&hr.to_le_bytes());
        buf[32..36].copy_from_slice(&br.to_le_bytes());
        buf[36..40].copy_from_slice(&dist.to_le_bytes());
        buf[40] = targets;
        buf[41] = 90; // confidence
        buf
    }

    #[test]
    fn mmwave_packet_routes_to_pending() {
        let mut state = BridgeState::from_config(&config());
        // node 1 is in "Den"; presence flag set, HR 72, BR 15, 1 target.
        let pkt = fused_vitals_bytes(1, 0x01, 72.0, 15.0, 320.0, 1);
        let n = state.ingest_datagram(&pkt);
        assert_eq!(n, 1);
        let drained = state.drain_mmwave();
        assert_eq!(drained.len(), 1);
        let (room, post) = &drained[0];
        assert_eq!(room, "Den");
        assert!(post.presence);
        assert_eq!(post.breathing_bpm, Some(15.0));
        assert_eq!(post.heart_rate_bpm, Some(72.0));
        assert_eq!(post.distance_cm, Some(320.0));
        // Draining clears it.
        assert!(state.drain_mmwave().is_empty());
    }

    #[test]
    fn mmwave_from_unmapped_node_is_dropped() {
        let mut state = BridgeState::from_config(&config());
        let pkt = fused_vitals_bytes(99, 0x01, 72.0, 15.0, 320.0, 1);
        assert_eq!(state.ingest_datagram(&pkt), 0);
        assert!(state.drain_mmwave().is_empty());
    }

    #[test]
    fn reduce_frame_mean_amplitude() {
        let (frames, _) = Esp32CsiParser::parse_stream(&frame_bytes(1, -50, &[(100, 0), (0, 50)]));
        let (amp, _phase) = reduce_frame(&frames[0]);
        // mean(|100|, |50|) = 75
        assert!((amp - 75.0).abs() < 0.01);
    }
}
