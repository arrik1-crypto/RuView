# RuView Tactical — Sensor Mesh Deployment Guide

How to build, provision, place, and run the ESP32 CSI mesh that drives the
tactical app. Pairs with the crate root docs and `android/README.md`.

> ⚠ **Decision-support only.** This system reports *anonymous human presence and
> coarse location*. It **cannot** tell a hostage from a suspect, confirm a weapon,
> count with certainty, or prove a room is empty. A still person, heavy
> construction, metal, or poor sensor geometry can all hide a real occupant.
> Every output is advisory and must be corroborated. Never the sole basis for a
> use-of-force decision.

---

## 1. Bill of materials

### Sensor nodes

| Item | Qty (single structure) | ~Cost ea | Why |
|------|-----------------------|----------|-----|
| **ESP32-S3 dev board, 8MB flash** (full-size) | 8–12 | $9 | The board the firmware is built and release-tested on. Dual-core Xtensa is **required** for the CSI DSP pipeline. 8MB flash carries OTA slots. |
| **ESP32-C6 + Seeed MR60BHA2** (60 GHz mmWave) | 1–2 | $15 | Optional. Adds an *independent* mmWave breathing/heart-rate/presence measurement to corroborate the CSI mesh on the priority room. |
| HLK-LD2410 (24 GHz presence) | 0–2 | $3 | Optional cheap presence-only tripwire; less useful than the above. |

> **Do NOT buy:** the original ESP32 or ESP32-C3 — single-core, cannot run the
> DSP pipeline (unsupported).
>
> **Avoid coin-sized clones** (SuperMini, ESP32-S3-Zero) for field use. The
> firmware keeps the radio on continuously (`WIFI_PS_NONE`) with a full DSP
> pipeline (`edge_tier=2`) — sustained high current draw. Compact boards with
> minimal PCB copper and budget regulators run hot, and at least one field report
> has one failing to power on again after a hot session. Full-size dev boards with
> real regulators only. Give every node airflow; check by touch in the first
> minutes of a deployment.

### Supporting kit

| Item | Qty | Purpose |
|------|-----|---------|
| USB power bank, 10 Ah+ | one per node | Nodes are USB-powered; outlets won't be where you need them |
| Short USB-C cables | one per node | — |
| Travel WiFi router **or** the phone's hotspot | 1 | Mesh + bridge host + phone/console must share one LAN |
| Raspberry Pi 5 or any laptop | 1 | Runs `ruview-csi-bridge`. A Pi 5 can later double as a nexmon capture node (`vendor/rvcsi`) |
| Labels + gaff tape | — | Label each board with its `node_id` **before** deployment |
| 2–3 spare nodes | — | Cheap boards fail; the mesh degrades gracefully but you'll want swaps |

**Budget:** roughly **$150–250** in boards + radar for a serious single-structure
kit, plus power banks and the Pi. The electronics are the cheap part.

---

## 2. How many nodes per room (the localization rule)

The number of nodes per room decides the fix quality — this is set by the
triangulator, not a preference:

| Nodes with live RSSI on a room | Result on the dashboard |
|--------------------------------|-------------------------|
| **≥ 3** | **Triangulated point fix** — a dot with an uncertainty ring |
| 1–2 | **Room-level presence only** — contact at the room centroid |
| 0 | No contact (which is **not** proof the room is empty) |

Practical placement:

- **Priority room** (suspected hostage room): **3+ nodes**, spread to the corners
  so the geometry is wide (not colinear). Add the ESP32-C6/mmWave node here.
- **Secondary rooms / halls**: 1–2 nodes for room-level presence.
- Nodes see *through* interior walls but are attenuated by them; place them on the
  room's own perimeter where possible, radio facing in.
- Spread nodes in X **and** Y. Three nodes in a straight line trilaterate poorly.

---

## 3. Build the firmware

Full build details live in `firmware/esp32-csi-node/README.md`. Summary:

```bash
# 8MB image (real WiFi CSI, no mock) — the standard node build
cd firmware/esp32-csi-node
# build per the firmware README (ESP-IDF v5.4)
```

- Build the **8MB** image from `sdkconfig.defaults.template` for 8MB boards, or the
  **4MB** image from `sdkconfig.defaults.4mb` for 4MB boards.
- **Always build the real-CSI image, never mock.** Mock mode has masked a real
  Kconfig threshold bug before — your acceptance test must be a live human, not the
  simulator.

---

## 4. Provision each node

Each board needs, at minimum: WiFi SSID/password, the aggregator IP, and a unique
`node_id`. Flash + provision over serial:

```bash
python firmware/esp32-csi-node/provision.py \
  --port /dev/ttyUSB0 \
  --ssid "TacticalNet" --password "secret" \
  --target-ip 192.168.50.10 \      # the bridge host's LAN IP (Pi/laptop/phone)
  --node-id 1                      # UNIQUE per node; maps to the bridge config
```

> **Port:** the firmware's default UDP target port (**5005**) and the bridge's
> default `listen` port (**5005**) match out of the box, so `--target-port` is
> optional. If you change one, change both — if they disagree, the bridge hears
> nothing.

> ⚠ **`node_id` is the contract.** The bridge config maps each `node_id` to a room
> and a sensor id. Label the physical board with its id and record which room it
> goes in *before* you're standing in a hallway under time pressure.

Provision every node against the same SSID and the same `--target-ip`. Increment
`--node-id` for each (1, 2, 3, …).

---

## 5. Map the mesh (bridge config)

Create the bridge config that ties `node_id` → room → sensor id. The `room` names
and `sensor_id` values must match the floor plan you load into the app (§7).

`csi-bridge.json` (see `examples/csi-bridge.example.json`):

```json
{
  "server": "http://192.168.50.20:8099",
  "listen": "0.0.0.0:5005",
  "flush_ms": 1000,
  "nodes": [
    { "node_id": 1, "room": "Bedroom 2", "sensor_id": "bed2-a", "primary": true },
    { "node_id": 2, "room": "Bedroom 2", "sensor_id": "bed2-b" },
    { "node_id": 3, "room": "Bedroom 2", "sensor_id": "bed2-c" },
    { "node_id": 4, "room": "Hallway",   "sensor_id": "hall-a", "primary": true },
    { "node_id": 5, "room": "Hallway",   "sensor_id": "hall-b" }
  ]
}
```

- **`server`** — where the tactical app is listening. If the app runs on the phone,
  this is the **phone's LAN IP** (the app binds `0.0.0.0:8099`).
- **`primary: true`** — one node per room drives the breathing/movement time-series.
  If you flag none, the first node listed for the room is used. Every node in the
  room contributes its RSSI for triangulation regardless.
- Nodes not listed here are ignored; sibling non-CSI packets are skipped.

---

## 6. Run the bridge

On the Pi/laptop (built with `--features bridge`):

```bash
cargo run -p wifi-densepose-tactical --features bridge --bin ruview-csi-bridge -- csi-bridge.json
# or the built binary:
./ruview-csi-bridge csi-bridge.json
```

It listens for ESP32 CSI over UDP and forwards distilled readings to the app's
`/api/csi`. You should see startup lines confirming the listen port, the target
`/api/csi` URL, and the node/room counts.

---

## 7. Load the floor plan and go live

1. Start the tactical app (the `ruview-tactical` binary, or the APK in live mode).
2. `POST /api/structure` with the real building layout — room bounds in metres and
   each room's sensor coordinates, with `id` values matching the bridge's
   `sensor_id`s. (The built-in demo house is fake; localization is only meaningful
   against real geometry.)
3. Open the dashboard (`http://<app-host>:8099/`). Confirm:
   - **Sensors panel** shows every node green/reporting with live RSSI.
   - Rooms with 3+ nodes read "point fix"; others "room-level".
4. **Acceptance test:** put a live person, breathing, behind the target wall.
   Confirm a contact appears in the right room with breathing confirmed. Do this
   with real CSI — never the simulator.

---

## 8. Field checklist

- [ ] Every node labeled with its `node_id`, matched to the bridge config.
- [ ] All nodes provisioned with the same SSID + the same `--target-ip` + matching port.
- [ ] Priority room has 3+ nodes in a wide (non-colinear) spread.
- [ ] Bridge `server` points at the app; `listen` port matches `--target-port`.
- [ ] Floor plan `sensor_id`s match bridge `sensor_id`s.
- [ ] Sensors panel all green before relying on the picture.
- [ ] Nodes have airflow; touch-check for heat in the first minutes.
- [ ] 2–3 spare provisioned nodes in the bag.
- [ ] Acceptance-tested against a live human, not mock/sim.

---

## 9. Data flow (reference)

```
ESP32-S3 nodes ──ADR-018 CSI over UDP──▶ ruview-csi-bridge ──POST /api/csi──▶ tactical app
   (node_id, RSSI, I/Q)                    (decode, reduce,        (MAT detection,
                                            per-room buffer)        localization)
                                                                        │
                                                          WebSocket ── dashboard
                                                                        │
                     phone BLE radio ──POST /api/ble──▶ app ── "devices, not people" overlay
```

- **CSI mesh** = the only path that senses *people* through walls.
- **BLE overlay** = the phone's own radio; detects *transmitting devices*, never
  people. Auxiliary, clearly separated, never on the floor plan.
- **mmWave (ESP32-C6/MR60BHA2)** = independent corroboration on the priority
  room. The C6 node emits an ADR-063 fused-vitals packet (`0xC5110004`) on the
  same UDP port as CSI; `ruview-csi-bridge` decodes it automatically and forwards
  to `POST /api/mmwave`. Map the C6's `node_id` to the room in the bridge config
  like any other node. The dashboard then shows a per-room badge:
  **✓ mmWave corroborated** (CSI + radar agree), **⚠ mmWave: clear (CSI contact)**
  (they disagree — investigate), or **mmWave presence (no CSI)**.
