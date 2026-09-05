// WebRTC peer setup for pyielink's media path.
//
// Replaces the WebSocket mux as the transport for VIDEO / AUDIO / INPUT / FILE
// channels (CONTROL/heartbeat stays on the ws). The existing Mux routes each
// channel to a live RTCDataChannel when one is attached, so VideoService,
// AudioService, InputService and FileService keep calling mux.send() unchanged.
//
// Signaling (SDP + ICE) rides the already-authenticated data-layer WebSocket as
// TEXT messages: {t:"rtc_offer",sdp} / {t:"rtc_answer",sdp} / {t:"rtc_ice",cand}
// If WebRTC fails to establish, callers simply never attach channels and the
// Mux keeps using the WebSocket for everything (PYIELINK_TRANSPORT=tcp).

import { RTCPeerConnection } from "werift";
import { CHANNELS } from "./mux.js";

function isValidUrl(url: string): boolean {
  try {
    new URL(url);
    return true;
  } catch {
    return false;
  }
}

function iceServers() {
  // Parse STUN URLs from env (pipe-separated), trim whitespace, validate
  const stunUrls = (process.env.PYIELINK_STUN || "")
    .split("|")
    .map((u) => u.trim())
    .filter((u) => u && isValidUrl(u));

  // Build output array with proper format
  const out: Array<{ urls: string | Array<string>; username?: string; credential?: string }> = [];

  // Add default STUN if none provided via env
  if (stunUrls.length === 0) {
    out.push({ urls: "stun:stun.l.google.com:19302" });
  } else {
    for (const u of stunUrls) {
      out.push({ urls: u });
    }
  }

  // Parse TURN entries: format is url|username|credential, entries separated by |
  // Allow multiple TURN entries by repeating the pattern: url1|user1|cred1|url2|user2|cred2|...
  const turn = process.env.PYIELINK_TURN;
  if (turn) {
    const turnEntries = turn.split("|");
    // Process in groups of 3: url, username, credential
    for (let i = 0; i + 2 < turnEntries.length; i += 3) {
      const url = turnEntries[i].trim();
      const username = turnEntries[i + 1]?.trim();
      const credential = turnEntries[i + 2]?.trim();
      if (url && isValidUrl(url)) {
        out.push({ urls: url, username, credential });
      }
    }
  }

  return out;
}

// label -> channels carried on that data channel
const LABELS = {
  video: [CHANNELS.VIDEO],
  audio: [CHANNELS.AUDIO],
  input: [CHANNELS.INPUT],
  file: [CHANNELS.FILE_META, CHANNELS.FILE_CHUNK],
};

function candidateToJSON(c: any): any {
  if (!c) return null;
  if (typeof c.toJSON === "function") return c.toJSON();
  if (c.candidate) return c; // already an object
  try { return JSON.parse(JSON.stringify(c)); } catch { return null; }
}

function attachDc(mux, dc, label) {
  const chans = LABELS[label];
  if (!chans) return;
  dc.binaryType = "arraybuffer";
  // A data-channel error must never crash the process: log it and let the
  // Mux fall back to the WebSocket for this channel.
  dc.on("error", (e) => mux.log?.(`[rtc] dc ${label} error: ${e?.message || e}`));
  dc.on("open", () => { for (const c of chans) mux.useRtcChannel(c, dc); });
  dc.on("close", () => { for (const c of chans) mux.useRtcChannel(c, null); });
}

// Host is the offerer: it has media to send, so it opens the connection.
export async function startHostRtc({ mux, onSignal, log }) {
  const pc = new RTCPeerConnection({ iceServers: iceServers() });
  for (const [label] of Object.entries(LABELS)) {
    const opts = (label === "video" || label === "audio")
      ? { ordered: false, maxRetransmits: 0 }
      : { ordered: true };
    const dc = pc.createDataChannel(label, opts);
    attachDc(mux, dc, label);
  }
  pc.on("icecandidate", (e) => {
    const cand = candidateToJSON(e.candidate);
    if (cand) onSignal({ t: "rtc_ice", cand });
  });
  pc.on("error", (e) => log?.(`[rtc] host pc error: ${e?.message || e}`));
  pc.on("connectionstatechange", () => log?.(`[rtc] host connectionState=${pc.connectionState}`));
  pc.on("iceconnectionstatechange", () => log?.(`[rtc] host iceState=${pc.iceConnectionState}`));
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  onSignal({ t: "rtc_offer", sdp: JSON.stringify(pc.localDescription) });
  log?.("[rtc] host offer created");
  return pc;
}

// Client is the answerer: it receives the offer, answers, and receives the
// data channels opened by the host.
export async function startClientRtc({ mux, onSignal, log }) {
  const pc = new RTCPeerConnection({ iceServers: iceServers() });
  pc.on("datachannel", (ev) => {
    const dc = ev && ev.channel ? ev.channel : ev;
    attachDc(mux, dc, dc.label);
  });
  pc.on("icecandidate", (e) => {
    const cand = candidateToJSON(e.candidate);
    if (cand) onSignal({ t: "rtc_ice", cand });
  });
  pc.on("error", (e) => log?.(`[rtc] client pc error: ${e?.message || e}`));
  pc.on("connectionstatechange", () => log?.(`[rtc] client connectionState=${pc.connectionState}`));
  pc.on("iceconnectionstatechange", () => log?.(`[rtc] client iceState=${pc.iceConnectionState}`));
  log?.("[rtc] client peer created, awaiting offer");
  return pc;
}

export async function applyHostSignal(pc, msg, log) {
  try {
    if (msg.t === "rtc_answer") {
      await pc.setRemoteDescription(JSON.parse(msg.sdp));
      log?.("[rtc] host applied answer");
    } else if (msg.t === "rtc_ice") {
      await pc.addIceCandidate(msg.cand);
    }
  } catch (e) {
    log?.(`[rtc] host signal error: ${e.message}`);
  }
}

export async function applyClientSignal(pc, msg, onSignal, log) {
  try {
    if (msg.t === "rtc_offer") {
      await pc.setRemoteDescription(JSON.parse(msg.sdp));
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      onSignal({ t: "rtc_answer", sdp: JSON.stringify(pc.localDescription) });
      log?.("[rtc] client answered");
    } else if (msg.t === "rtc_ice") {
      await pc.addIceCandidate(msg.cand);
    }
  } catch (e) {
    log?.(`[rtc] client signal error: ${e.message}`);
  }
}