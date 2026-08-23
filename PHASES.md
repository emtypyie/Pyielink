# Pyielink Development Phases

## Phase 3 — COMPLETE ✅
- File Transfer (PUT/GET, resume, zero-byte, sandbox)
- Input Reflection (low-level hooks, REPL `input start|stop`)
- Screen Streaming (FFmpeg → MPEG-TS → openh264 → wgpu)
- All regression tests passing

---

## Phase 4 — Session & Connection Management

### 4.1 Session Reconnection
- Detect disconnect in `data_link_loop` (WS close, heartbeat timeout)
- Persist session state: video decoder state, input capture state, file transfer offsets
- Auto-reconnect to same data port (ticket is single-use, need re-promotion flow)
- Resume video stream from last keyframe (request IDR from server)
- Resume input capture automatically

**Files to modify:**
- `pyielink-rs/src/client.rs` - `data_link_loop` reconnection logic
- `pyielink-rs/src/session.rs` - new session state struct
- `datalayer/src/server.js` - ticket re-issue on reconnect attempt

### 4.2 Multi-Session Support
- Host accepts multiple client connections on same data port
- Each client gets isolated `VideoService` + `InputService` instances
- Mux channels include session ID in frame header
- Server tracks active sessions, broadcasts heartbeats per session

**Files to modify:**
- `datalayer/src/server.js` - session registry, per-session services
- `datalayer/src/video.js` - session-aware FFmpeg spawning
- `pyielink-rs/src/client.rs` - session ID in handshake

### 4.3 Persistent Auth Tokens
- Store 64-hex token + hostname in `~/.config/pyielink/tokens.json`
- `client connect <target>` reads token if present, skips password prompt
- Token rotation on successful proof (already implemented server-side)
- Encrypt token file with OS keyring (Windows DPAPI via `keyring` crate)

**Files to create:**
- `pyielink-rs/src/auth_store.rs` - token persistence
- `pyielink-rs/src/client.rs` - token lookup before auth flow

---

## Phase 5 — Audio Streaming

### 5.1 Server Audio Capture
- New `audio.js` service, mirrors `video.js` pattern
- FFmpeg: `ffmpeg -f dshow -i audio="Microphone" -c:a libopus -f opus pipe:1`
- Push Opus frames to `CHANNELS.AUDIO = 0x04`
- Handle `audio_start` / `audio_stop` control messages

**Files to create:**
- `datalayer/src/audio.js`

**Files to modify:**
- `datalayer/src/server.js` - register `AudioService`

### 5.2 Client Audio Playback
- New channel `DL_CH_AUDIO = 0x04` in `client.rs`
- Decode Opus with `opus` crate (or `libopus` bindings)
- Play via `rodio` or `cpal` output stream
- Buffer management for low latency (<50ms)

**Files to create:**
- `pyielink-rs/src/audio.rs` - decoder + playback
- `pyielink-gui/src/audio_player.rs` - GUI integration

---

## Phase 6 — Multi-Monitor & Focus

### 6.1 Multi-Monitor Enumeration
- `wgpu::Instance::enumerate_adapters()` + `Adapter::get_info()`
- Map to Windows display devices via `EnumDisplayDevicesW`
- GUI dropdown: "Monitor 1 (1920x1080)", "Monitor 2 (2560x1440)", "All"
- Server FFmpeg: `-offset_x -offset_y -video_size WxH` per monitor

**Files to modify:**
- `pyielink-gui/src/main.rs` - monitor selection UI
- `datalayer/src/video.js` - per-monitor FFmpeg args

### 6.2 Focus-Gated Capture
- Client tracks window focus state (`egui::Context::input().focused`)
- Send `video_pause` / `video_resume` control messages on focus loss/gain
- Server pauses FFmpeg (SIGSTOP) or drops frames
- On-screen indicator: "● LIVE" vs "○ PAUSED"

**Files to modify:**
- `pyielink-gui/src/main.rs` - focus tracking + control messages
- `datalayer/src/video.js` - handle pause/resume

---

## Phase 7 — Performance & Adaptive Streaming

### 7.1 Adaptive Bitrate
- Client measures RTT + throughput (ACK timing + frame sizes)
- Send `bitrate_request <kbps>` control message every 5s
- Server adjusts FFmpeg `-b:v` on the fly (restart encoder or use `filter_complex`)
- Target: maintain <100ms glass-to-glass latency

**Files to modify:**
- `pyielink-rs/src/client.rs` - bandwidth estimator
- `datalayer/src/video.js` - dynamic bitrate adjustment

### 7.2 Frame Pacing
- Server timestamps each frame (PTS in MPEG-TS)
- Client measures decode + render latency
- Drop late frames, duplicate if behind
- Target steady 30/60 FPS output

**Files to modify:**
- `pyielink-gui/src/video_decoder.rs` - frame queue + pacing logic

---

## Phase 8 — UX Polish

### 8.1 In-App File Browser
- Replace REPL `put`/`get` with egui file tree
- Left pane: local FS, Right pane: remote FS
- Drag-drop transfers, progress bars, resume buttons

**Files to create:**
- `pyielink-gui/src/file_browser.rs`

### 8.2 Session Recording
- Client option: "Record Session"
- Save raw MPEG-TS to `~/Videos/pyielink-<timestamp>.ts`
- Post-process: `ffmpeg -i input.ts -c copy output.mp4`

**Files to modify:**
- `pyielink-gui/src/main.rs` - recording toggle + file writer

### 8.3 On-Screen OSD
- Corner overlay: `Bitrate: 4.2 Mbps | FPS: 29.8 | Latency: 67ms | Input: ON`
- Update every second from client metrics
- Toggle with `Tab` key

**Files to modify:**
- `pyielink-gui/src/main.rs` - OSD render in `update()`

---

## Dependency Additions Needed

| Phase | Crate | Purpose |
|-------|-------|---------|
| 4.3 | `keyring` | OS credential storage |
| 5.2 | `opus`, `rodio`/`cpal` | Audio decode + playback |
| 6.1 | `windows` (additional) | Display device enumeration |
| 8.1 | `walkdir`, `chrono` | File tree + timestamps |

---

## Test Suite Additions

- `test-reconnect.ps1` — disconnect/reconnect flow
- `test-multisession.ps1` — two clients one host
- `test-audio.ps1` — audio round-trip
- `test-multimonitor.ps1` — monitor selection
- `test-adaptive.ps1` — bandwidth throttling simulation