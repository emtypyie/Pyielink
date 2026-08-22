use crate::creds;
use crate::proto::{self, *};
use crate::token;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::{Message, WebSocket};

const BOOTSTRAP_PORT: u16 = 4242;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HB_STALE: Duration = Duration::from_secs(20);
/// data-link lifecycle: 0 = connecting/authenticating, 1 = up, 2 = dead
const DL_CONNECTING: u8 = 0;
const DL_UP: u8 = 1;
const DL_DEAD: u8 = 2;
const DL_POLL: Duration = Duration::from_millis(200);
const DL_STALE: Duration = Duration::from_secs(15);
const DL_PING_EVERY: Duration = Duration::from_secs(5);
/// file-transfer channels (mirrors datalayer/src/mux.js CHANNELS)
const DL_CH_META: u8 = 0x04;
const DL_CH_CHUNK: u8 = 0x05;
/// chunk size + how many chunks we push per service-loop pass so dl
/// heartbeats keep flowing while an upload drains (backpressure is the
/// blocking TCP write; pacing keeps the control loop alive)
const XFER_CHUNK: usize = 64 * 1024;
const XFER_PUMP_CHUNKS: usize = 8;
/// xfer_status values: 0 running/none, 1 ok, 2 failed
const XFER_RUN: u8 = 0;
const XFER_OK: u8 = 1;
const XFER_FAIL: u8 = 2;

pub enum DlCommand {
    Get { remote: String, local: PathBuf },
    Put { local: PathBuf, remote: String },
}

#[derive(Clone)]
pub enum RunMode {
    Shell,
    OneShotGet { remote: String, local: PathBuf },
    OneShotPut { local: PathBuf, remote: String },
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// minimal JSON field extraction for our own well-known server messages;
/// string values come back unescaped enough for paths/codes we emit
fn jstr(json: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\":", key);
    let start = json.find(&pat)? + pat.len();
    let rest = &json[start..];
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = stripped.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return Some(out),
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let hex: String = chars.by_ref().take(4).collect();
                        out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                    }
                    other => out.push(other),
                },
                c => out.push(c),
            }
        }
        None
    } else {
        let end = rest.find(|c: char| c == ',' || c == '}').unwrap_or(rest.len());
        Some(rest[..end].trim().trim_matches('"').to_string())
    }
}

fn jnum<T: std::str::FromStr>(json: &str, key: &str) -> Option<T> {
    jstr(json, key)?.trim().parse().ok()
}

struct ActiveGet {
    local: PathBuf,
    file: Option<File>,
    written: u64,
    size: u64,
    expect_sha: String,
    hasher: Sha256,
    seeded: bool,
    last_pct: i64,
}

struct ActivePut {
    path: PathBuf,
    reader: Option<BufReader<File>>,
    next_offset: u64,
    size: u64,
    awaiting_done: bool,
    last_pct: i64,
}

enum Active {
    Get { label: String, tx: ActiveGet },
    Put { label: String, tx: ActivePut },
}

pub fn parse_target(arg: &str) -> Result<(String, String), String> {
    let (user, ip) = arg
        .split_once('@')
        .ok_or_else(|| format!("invalid target '{}': expected user@ip", arg))?;
    if user.is_empty() || ip.is_empty() {
        return Err(format!("invalid target '{}': expected user@ip", arg));
    }
    Ok((user.to_string(), ip.to_string()))
}

fn prompt_password() -> String {
    print!("password: ");
    let _ = std::io::stdout().flush();
    creds::read_masked()
}

fn password_proof(salt: &str, nonce: &str) -> String {
    let pw = prompt_password();
    creds::compute_proof(&creds::hash_password(salt, &pw), nonce)
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

pub fn run_connect(target: &str) -> Result<(), String> {
    run_session(target, RunMode::Shell)
}

/// one-shot download: auth, transfer, BYE — fully scriptable
pub fn run_get(target: &str, remote: &str, local: Option<&str>) -> Result<(), String> {
    let local = match local {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(basename_of(remote)),
    };
    run_session(
        target,
        RunMode::OneShotGet {
            remote: remote.to_string(),
            local,
        },
    )
}

/// one-shot upload: auth, transfer, BYE
pub fn run_put(target: &str, local: &str, remote: Option<&str>) -> Result<(), String> {
    if !local.is_empty() && !std::path::Path::new(local).is_file() {
        return Err(format!("no such file: {}", local));
    }
    let remote = match remote {
        Some(r) => r.to_string(),
        None => basename_of(local).to_string(),
    };
    run_session(
        target,
        RunMode::OneShotPut {
            local: PathBuf::from(local),
            remote,
        },
    )
}

fn basename_of(p: &str) -> &str {
    let p = p.trim_end_matches(['\\', '/']);
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

fn run_session(target: &str, mode: RunMode) -> Result<(), String> {
        let (user, ip) = parse_target(target)?;
        // tests / multi-host machines can redirect the bootstrap port
        let port: u16 = std::env::var("PYIELINK_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(BOOTSTRAP_PORT);
        let addr = format!("{}:{}", ip, port);
    println!("  [..] connecting to {} as '{}' ...", addr, user);

    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| format!("cannot reach {} — is the host running /enable and its firewall open? ({})", addr, e))?;
    stream
        .set_nodelay(true)
        .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|e| e.to_string())?;
    if let Ok(local) = stream.local_addr() {
        println!("  [ok] connected {} -> {}", local, addr);
    } else {
        println!("  [ok] connected to {}", addr);
    }

    let hello = format!("{}\n{}\n", user, env!("CARGO_PKG_VERSION"));
    proto::write_frame(&mut stream, HELLO, hello.as_bytes())
        .map_err(|e| format!("handshake send failed: {}", e))?;

    // Auth: answer challenges with proofs; secrets never leave this machine.
    // p-mode: derive the same iterated hash the host stores, prove over that.
    // t-mode: the stored token file holds sha256(token); prove over that.
    let mut sent_token_proof = false;
    let mut attempts = 0u32;
    loop {
        match expect_frame(&mut stream)? {
            (CHALLENGE, payload) => {
                let line = String::from_utf8_lossy(&payload).into_owned();
                let (salt, nonce) = match line.split_once('\n') {
                    Some(x) => (x.0.to_string(), x.1.trim().to_string()),
                    None => return Err("malformed challenge from host".into()),
                };
                let (mode, proof) = if !sent_token_proof {
                    sent_token_proof = true;
                    match token::load_client_token(&user, &ip).filter(|t| t.len() == 64) {
                        Some(tok_hash) => ("t", creds::compute_proof(&tok_hash, &nonce)),
                        None => {
                            // one-shot / scripted runs must never block on a
                            // hidden console read: fail fast instead of hang
                            if !creds::stdin_is_tty()
                                && std::env::var("PYIELINK_SHELL").as_deref() != Ok("1")
                            {
                                return Err(format!(
                                    "no stored credential for {}@{} - run an interactive session once to store a token",
                                    user, ip
                                ));
                            }
                            ("p", password_proof(&salt, &nonce))
                        }
                    }
                } else {
                    if attempts >= 3 {
                        return Err("too many failed authentication attempts".into());
                    }
                    attempts += 1;
                    ("p", password_proof(&salt, &nonce))
                };
                let framed = format!("{}:{}", mode, proof);
                proto::write_frame(&mut stream, PROOF, framed.as_bytes())
                    .map_err(|e| format!("send failed: {}", e))?;
            }
            (LICENSE_TEXT, payload) => {
                if license_preaccepted() {
                    println!("  [i] agreement pre-accepted via PYIELINK_ACCEPT_LICENSE (you are accountable for authorization)");
                } else {
                    println!("\n{}", String::from_utf8_lossy(&payload));
                    if !confirm_license() {
                        proto::write_frame(&mut stream, LICENSE_REJECT, b"n")
                            .map_err(|e| e.to_string())?;
                        return Err("license rejected — session aborted".into());
                    }
                }
                proto::write_frame(&mut stream, LICENSE_ACCEPT, b"y")
                    .map_err(|e| e.to_string())?;
            }
            (TOKEN_ISSUED, payload) => {
                let tok = String::from_utf8_lossy(&payload).trim().to_string();
                let path = token::save_client_token(&user, &ip, &token::hash_token(&tok))
                    .map_err(|e| format!("could not store token: {}", e))?;
                println!("  [ok] connection credential stored at {}", path.display());
            }
            (AUTH_OK, payload) => {
                let ticket = String::from_utf8_lossy(&payload).into_owned();
                let (data_port, session_key) = split_ticket(ticket.trim())?;
                println!(
                    "  [ok] session promoted. data layer ready on {}:{}. session key received.",
                    ip, data_port
                );
                let dl_state = Arc::new(AtomicU8::new(DL_CONNECTING));
                let dl_stop = Arc::new(AtomicU8::new(0));
                let xfer_status = Arc::new(AtomicU8::new(XFER_RUN));
                let (xfer_tx, xfer_rx) = mpsc::channel::<DlCommand>();
                let dl_handle = {
                    let (ip, port, key, state, stop, status) = (
                        ip.clone(),
                        data_port.clone(),
                        session_key.clone(),
                        Arc::clone(&dl_state),
                        Arc::clone(&dl_stop),
                        Arc::clone(&xfer_status),
                    );
                    std::thread::spawn(move || {
                        data_link_loop(&ip, &port, &key, state, stop, xfer_rx, Some(status))
                    })
                };
                if let Err(e) = match &mode {
                    RunMode::OneShotGet { remote, local } => xfer_tx.send(DlCommand::Get {
                        remote: remote.clone(),
                        local: local.clone(),
                    }),
                    RunMode::OneShotPut { local, remote } => xfer_tx.send(DlCommand::Put {
                        local: local.clone(),
                        remote: remote.clone(),
                    }),
                    RunMode::Shell => Ok(()),
                } {
                    eprintln!("  [xfer] data link unavailable: {}", e);
                }
                let interactive = std::env::var("PYIELINK_SHELL").ok().as_deref() == Some("1")
                    || creds::stdin_is_tty();
                if interactive && matches!(mode, RunMode::Shell) {
                    println!("  [i] remote terminal ready — type a command ('sudo <cmd>' for elevated, 'get'/'put' to transfer, 'exit' to quit)");
                    print!("pyielink> ");
                    let _ = std::io::stdout().flush();
                }
                let outcome =
                    post_auth_loop(&mut stream, interactive, &dl_state, &xfer_tx, &xfer_status, &mode);
                dl_stop.store(1, Ordering::Relaxed);
                let _ = dl_handle.join();
                outcome?;
                if matches!(mode, RunMode::Shell) || xfer_status.load(Ordering::Relaxed) == XFER_OK {
                    return Ok(());
                }
                return Err("file transfer failed".into());
            }
            (AUTH_FAIL, payload) => {
                return Err(format!(
                    "host refused: {}",
                    String::from_utf8_lossy(&payload).trim()
                ));
            }
            (msg, _) => {
                return Err(format!("unexpected frame 0x{:02X} during handshake", msg));
            }
        }
    }
}

fn split_ticket(ticket: &str) -> Result<(String, String), String> {
    match ticket.split_once('\n') {
        Some((p, k)) if !p.is_empty() && k.len() == 64 => Ok((p.to_string(), k.to_string())),
        _ => Err("host sent malformed promotion ticket".into()),
    }
}

fn license_preaccepted() -> bool {
    matches!(
        std::env::var("PYIELINK_ACCEPT_LICENSE").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    )
}

enum StdinMsg {
    Line(String),
    Eof,
}

/// Blocking stdin reader on its own thread; the main loop polls it between
/// socket frames so heartbeats stay responsive while waiting for input.
fn spawn_stdin_reader() -> std::sync::mpsc::Receiver<StdinMsg> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(StdinMsg::Eof);
                    return;
                }
                Ok(_) => {
                    if tx.send(StdinMsg::Line(line.clone())).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// mux framing identical to datalayer/src/mux.js: [u8 channel][u32 len BE][payload]
fn dl_frame(channel: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(channel);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn dl_parse(buf: &[u8]) -> Option<(u8, &[u8])> {
    if buf.len() < 5 {
        return None;
    }
    let ch = buf[0];
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return None;
    }
    Some((ch, &buf[5..5 + len]))
}

/// Native data-plane client: ws handshake with the session key, control-
/// channel heartbeat service (answer PINGs, emit own PINGs, RTT log) under a
/// staleness watchdog, plus the file-transfer engine (Phase 3.1). Exits when
/// `stop` flips to 1 or the link dies.
#[allow(clippy::too_many_arguments)]
fn data_link_loop(
    ip: &str,
    port: &str,
    key: &str,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicU8>,
    cmds: mpsc::Receiver<DlCommand>,
    status: Option<Arc<AtomicU8>>,
) {
    let addr = format!("{}:{}", ip, port);
    let tcp = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  [dl-link] cannot reach {}: {}", addr, e);
            state.store(DL_DEAD, Ordering::Relaxed);
            return;
        }
    };
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(DL_POLL));
    let _ = tcp.set_write_timeout(Some(CONNECT_TIMEOUT));
    let url = format!("ws://{}/", addr);
    let mut ws: WebSocket<TcpStream> = match tungstenite::client(url, tcp) {
        Ok((ws, _)) => ws,
        Err(e) => {
            eprintln!("  [dl-link] websocket handshake failed: {}", e);
            state.store(DL_DEAD, Ordering::Relaxed);
            return;
        }
    };

    // first message must be the session key; server answers a plain JSON ack
    if ws.send(Message::Text(format!("{{\"k\":\"{}\"}}", key))).is_err() {
        eprintln!("  [dl-link] failed to send session key");
        state.store(DL_DEAD, Ordering::Relaxed);
        return;
    }
    loop {
        if stop.load(Ordering::Relaxed) == 1 {
            dl_shutdown(&mut ws);
            return;
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                if t.contains("\"ok\":true") || t.contains("\"ok\": true") {
                    println!("  [dl-link] data channel up ({})", t.trim());
                    break;
                }
                eprintln!("  [dl-link] unexpected ack: {}", t.trim());
                state.store(DL_DEAD, Ordering::Relaxed);
                return;
            }
            Ok(Message::Close(c)) => {
                eprintln!(
                    "  [dl-link] rejected by data layer (code {})",
                    c.map(|f| u16::from(f.code)).unwrap_or(0)
                );
                state.store(DL_DEAD, Ordering::Relaxed);
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(e) => {
                eprintln!("  [dl-link] auth read failed: {}", e);
                state.store(DL_DEAD, Ordering::Relaxed);
                return;
            }
        }
    }
    state.store(DL_UP, Ordering::Relaxed);

    // control-channel heartbeat: answer server PINGs, send our own, log RTT
    let mut last_seen = Instant::now();
    let mut next_ping = Instant::now() + DL_PING_EVERY;
    let mut awaiting_pong_at: Option<(u128, Instant)> = None;
    // file-transfer state (Phase 3.1)
    let mut transfers: HashMap<u32, Active> = HashMap::new();
    let mut next_id: u32 = 1;
    loop {
        if stop.load(Ordering::Relaxed) == 1 {
            if !transfers.is_empty() {
                eprintln!("  [xfer] session ended with {} transfer(s) incomplete", transfers.len());
                for (_, a) in &transfers {
                    eprintln!("  [xfer] partial kept for resume: {}", a.label());
                }
            }
            dl_shutdown(&mut ws);
            return;
        }
        while let Ok(cmd) = cmds.try_recv() {
            if let Err(e) = start_cmd(&mut ws, &mut next_id, &mut transfers, cmd) {
                eprintln!("  [xfer] cannot start: {}", e);
                finish_oneshot(&status, false);
            }
        }
        pump_puts(&mut ws, &mut transfers);
        match ws.read() {
            Ok(Message::Binary(buf)) => match dl_parse(&buf) {
                Some((0x01, payload)) if payload.first() == Some(&b'P') => {
                    last_seen = Instant::now();
                    let _ = ws.send(Message::Binary(dl_frame(0x01, payload))); // PONG echoes nonce
                }
                Some((0x01, payload)) if payload.first() == Some(&b'Q') => {
                    last_seen = Instant::now();
                    if let Some((sent_ms, _)) =
                        awaiting_pong_at.take_if(|(ms, _)| payload.get(1..) == Some(ms.to_string().as_bytes()))
                    {
                        println!("  [dl-hb] rtt {}ms", now_ms().saturating_sub(sent_ms));
                    }
                }
                Some((DL_CH_META, payload)) => handle_meta_json(payload, &mut ws, &mut transfers, &status),
                Some((DL_CH_CHUNK, payload)) => handle_chunk(payload, &mut ws, &mut transfers, &status),
                _ => {} // unknown channel: drop silently (mirrors mux policy)
            },
            Ok(Message::Close(_)) | Ok(Message::Frame(_)) => {
                println!("  [dl-link] data layer closed the channel");
                state.store(DL_DEAD, Ordering::Relaxed);
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if last_seen.elapsed() >= DL_STALE {
                    eprintln!("  [dl-link] data layer stopped responding — tearing down");
                    state.store(DL_DEAD, Ordering::Relaxed);
                    return;
                }
                if next_ping.elapsed() >= Duration::from_secs(0) {
                    let ms = now_ms();
                    if ws.send(Message::Binary(dl_frame(0x01, format!("P{}", ms).as_bytes()))).is_err() {
                        eprintln!("  [dl-link] ping send failed");
                        state.store(DL_DEAD, Ordering::Relaxed);
                        return;
                    }
                    awaiting_pong_at = Some((ms, Instant::now()));
                    next_ping += DL_PING_EVERY;
                }
            }
            Err(e) => {
                // a one-shot transfer that already finished tears the socket
                // down under us; that close race is expected, not an error
                let finished = status
                    .as_ref()
                    .map(|s| s.load(Ordering::SeqCst) != XFER_RUN)
                    .unwrap_or(false);
                if !finished {
                    eprintln!("  [dl-link] read failed: {}", e);
                }
                state.store(DL_DEAD, Ordering::Relaxed);
                return;
            }
        }
    }
}

fn dl_shutdown(ws: &mut WebSocket<TcpStream>) {
    let _ = ws.close(None);
    let _ = ws.flush();
}

// ---------- file-transfer engine (Phase 3.1) ----------

impl Active {
    fn label(&self) -> &str {
        match self {
            Active::Get { label, .. } | Active::Put { label, .. } => label,
        }
    }
}

fn finish_oneshot(status: &Option<Arc<AtomicU8>>, ok: bool) {
    if let Some(s) = status {
        let want = if ok { XFER_OK } else { XFER_FAIL };
        s.compare_exchange(XFER_RUN, want, Ordering::Relaxed, Ordering::Relaxed)
            .ok();
    }
}

fn send_meta_json(ws: &mut WebSocket<TcpStream>, json: String) -> Result<(), String> {
    ws.send(Message::Binary(dl_frame(DL_CH_META, json.as_bytes())))
        .map_err(|e| format!("meta send failed: {}", e))
}

/// REPL verbs: `get <remote> [local]` / `put <local> [remote]`
fn handle_xfer_verb(line: &str, tx: &mpsc::Sender<DlCommand>) {
    let mut it = line.split_whitespace();
    let verb = it.next().unwrap_or("");
    let rest: Vec<&str> = it.collect();
    let cmd = match (verb, rest.len()) {
        ("get", 1) => Some(DlCommand::Get {
            remote: rest[0].to_string(),
            local: PathBuf::from(basename_of(rest[0])),
        }),
        ("get", 2) => Some(DlCommand::Get {
            remote: rest[0].to_string(),
            local: PathBuf::from(rest[1]),
        }),
        ("put", 1) => {
            if std::path::Path::new(rest[0]).is_file() {
                Some(DlCommand::Put {
                    local: PathBuf::from(rest[0]),
                    remote: basename_of(rest[0]).to_string(),
                })
            } else {
                println!("  [xfer] no such file: {}", rest[0]);
                None
            }
        }
        ("put", 2) => {
            if std::path::Path::new(rest[0]).is_file() {
                Some(DlCommand::Put {
                    local: PathBuf::from(rest[0]),
                    remote: rest[1].to_string(),
                })
            } else {
                println!("  [xfer] no such file: {}", rest[0]);
                None
            }
        }
        _ => None,
    };
    match cmd {
        Some(c) => {
            if tx.send(c).is_err() {
                println!("  [xfer] data link not available");
            } else {
                println!("  [xfer] queued.");
            }
        }
        None => {
            if verb == "get" || verb == "put" {
                println!("  [i] usage: {} <file> [destination]", verb);
            } else {
                println!("  [i] usage: get <remote> [local] | put <local> [remote]");
            }
        }
    }
}

fn start_cmd(
    ws: &mut WebSocket<TcpStream>,
    next_id: &mut u32,
    transfers: &mut HashMap<u32, Active>,
    cmd: DlCommand,
) -> Result<(), String> {
    match cmd {
        DlCommand::Get { remote, local } => {
            let have = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);
            let id = *next_id;
            *next_id += 1;
            transfers.insert(
                id,
                Active::Get {
                    label: format!("{} -> {}", remote, local.display()),
                    tx: ActiveGet {
                        local,
                        file: None,
                        written: have,
                        size: 0,
                        expect_sha: String::new(),
                        hasher: Sha256::new(),
                        seeded: false,
                        last_pct: -1,
                    },
                },
            );
            send_meta_json(
                ws,
                format!(
                    "{{\"t\":\"pull\",\"id\":{},\"name\":\"{}\",\"have\":{}}}",
                    id,
                    json_escape(&remote),
                    have
                ),
            )?;
            if have > 0 {
                println!("  [xfer] GET '{}' (resuming at {} bytes)", remote, have);
            } else {
                println!("  [xfer] GET '{}'", remote);
            }
            Ok(())
        }
        DlCommand::Put { local, remote } => {
            let mut f =
                File::open(&local).map_err(|e| format!("cannot open {}: {}", local.display(), e))?;
            let size = f.metadata().map_err(|e| e.to_string())?.len();
            // announce sha256 of the whole file up front; host verifies on arrival
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; XFER_CHUNK];
            loop {
                let n = f.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let expect = hex(&hasher.finalize());
            let id = *next_id;
            *next_id += 1;
            transfers.insert(
                id,
                Active::Put {
                    label: format!("{} -> {}", local.display(), remote),
                    tx: ActivePut {
                        path: local.clone(),
                        reader: None,
                        next_offset: 0,
                        size,
                        awaiting_done: false,
                        last_pct: 0,
                    },
                },
            );
            send_meta_json(
                ws,
                format!(
                    "{{\"t\":\"push\",\"id\":{},\"name\":\"{}\",\"size\":{},\"sha256\":\"{}\"}}",
                    id,
                    json_escape(&remote),
                    size,
                    expect
                ),
            )?;
            println!("  [xfer] PUT '{}' ({} bytes)", remote, size);
            Ok(())
        }
    }
}

/// push pending upload chunks — bounded per pass so dl heartbeats keep flowing
fn pump_puts(ws: &mut WebSocket<TcpStream>, transfers: &mut HashMap<u32, Active>) {
    let ids: Vec<u32> = transfers
        .iter()
        .filter(|(_, a)| match a {
            Active::Put { tx, .. } => tx.reader.is_some() && !tx.awaiting_done,
            _ => false,
        })
        .map(|(k, _)| *k)
        .collect();
    for id in ids {
        for _ in 0..XFER_PUMP_CHUNKS {
            // stage one chunk under a short-lived borrow, then send outside it
            let staged = 'stage: {
                let tx = match transfers.get_mut(&id) {
                    Some(Active::Put { tx, .. }) if !tx.awaiting_done => tx,
                    _ => break 'stage None,
                };
                let reader = match tx.reader.as_mut() {
                    Some(r) => r,
                    None => break 'stage None,
                };
                let mut buf = vec![0u8; XFER_CHUNK];
                let n = match reader.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        println!("  [xfer] '{}' read failed mid-upload: {}", id, e);
                        break 'stage None;
                    }
                };
                if n == 0 {
                    tx.awaiting_done = true;
                    // announce end-of-stream; essential for zero-byte pushes
                    // where no FILE_CHUNK ever triggers host-side completion
                    let sent = tx.next_offset;
                    let note = format!("{{\"t\":\"eof\",\"id\":{},\"bytes\":{}}}", id, sent);
                    if ws.send(Message::Binary(dl_frame(DL_CH_META, note.as_bytes()))).is_err() {
                        println!("  [xfer] upload '{}' eof send failed", id);
                    }
                    break 'stage None;
                }
                let off = tx.next_offset;
                tx.next_offset += n as u64;
                let pct = if tx.size > 0 { ((tx.next_offset.min(tx.size)) * 100 / tx.size) as i64 } else { 100 };
                let announce = tx.next_offset >= tx.size || pct >= tx.last_pct + 10;
                if announce {
                    tx.last_pct = pct;
                }
                let mut p = Vec::with_capacity(12 + n);
                p.extend_from_slice(&id.to_be_bytes());
                p.extend_from_slice(&off.to_be_bytes());
                p.extend_from_slice(&buf[..n]);
                break 'stage Some((pct, announce, p));
            };
            if let Some((pct, announce, p)) = staged {
                if ws.send(Message::Binary(dl_frame(DL_CH_CHUNK, &p))).is_err() {
                    println!("  [xfer] upload '{}' send failed", id);
                    break;
                }
                if announce {
                    println!("  [xfer] PUT #{} {}%", id, pct);
                }
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// host->client control messages on the file_meta channel
fn handle_meta_json(
    payload: &[u8],
    ws: &mut WebSocket<TcpStream>,
    transfers: &mut HashMap<u32, Active>,
    status: &Option<Arc<AtomicU8>>,
) {
    let json = String::from_utf8_lossy(payload);
    let t = match jstr(&json, "t") {
        Some(t) => t,
        None => return,
    };
    let id: u32 = match jnum(&json, "id") {
        Some(id) => id,
        None => return,
    };
    match t.as_str() {
        "meta" => {
            // pull accepted: announced size + full-file sha256
            let size: u64 = jnum(&json, "size").unwrap_or(0);
            let expect = jstr(&json, "sha256").unwrap_or_default();
            if let Some(Active::Get { label, tx }) = transfers.get_mut(&id) {
                if tx.file.is_some() {
                    return; // duplicate meta: ignore
                }
                match OpenOptions::new().create(true).append(true).open(&tx.local) {
                    Ok(f) => {
                        if !tx.seeded && tx.written > 0 {
                            // hash the prefix we already hold so the final
                            // digest covers the assembled whole file
                            if let Ok(mut pf) = File::open(&tx.local) {
                                let mut buf = vec![0u8; XFER_CHUNK];
                                loop {
                                    let n = pf.read(&mut buf).unwrap_or(0);
                                    if n == 0 {
                                        break;
                                    }
                                    tx.hasher.update(&buf[..n]);
                                }
                            }
                        }
                        tx.seeded = true;
                        tx.size = size;
                        tx.expect_sha = expect;
                        println!(
                            "  [xfer] GET '{}' ({} bytes{})",
                            label,
                            size,
                            if tx.written > 0 { format!(", resuming at {}", tx.written) } else { String::new() }
                        );
                        tx.file = Some(f);
                    }
                    Err(e) => {
                        println!("  [xfer] cannot open '{}': {}", tx.local.display(), e);
                        transfers.remove(&id);
                        finish_oneshot(status, false);
                    }
                }
            } else {
                println!("  [xfer] meta for unknown transfer #{}", id);
            }
        }
        "ready" => {
            // push accepted: stream from the resume offset the host reports
            let resume: u64 = jnum(&json, "resume").unwrap_or(0);
            let size: u64 = jnum(&json, "size").unwrap_or(0);
            if let Some(Active::Put { label, tx }) = transfers.get_mut(&id) {
                match File::open(&tx.path) {
                    Ok(mut f) => {
                        let start = resume.min(tx.size);
                        if f.seek(SeekFrom::Start(start)).is_ok() {
                            tx.reader = Some(BufReader::new(f));
                            tx.next_offset = start;
                            println!(
                                "  [xfer] PUT '{}' streaming from {} ({} bytes total)",
                                label, start, size
                            );
                        } else {
                            println!("  [xfer] PUT '{}' seek failed", label);
                            transfers.remove(&id);
                            finish_oneshot(status, false);
                        }
                    }
                    Err(e) => {
                        println!("  [xfer] PUT '{}' reopen failed: {}", label, e);
                        transfers.remove(&id);
                        finish_oneshot(status, false);
                    }
                }
            } else {
                println!("  [xfer] ready for unknown transfer #{}", id);
            }
        }
        "eof" => {
            // host finished queueing its stream; zero-remainder resumes end here
            if let Some(Active::Get { .. }) = transfers.get(&id) {
                let done = match transfers.get(&id) {
                    Some(Active::Get { tx, .. }) => tx.size > 0 && tx.written >= tx.size || tx.size == 0,
                    _ => false,
                };
                if done {
                    finalize_get(ws, transfers, status, id);
                }
            }
        }
        "done" => {
            let ok = jstr(&json, "ok").as_deref() == Some("true");
            if let Some(a) = transfers.remove(&id) {
                println!(
                    "  [xfer] PUT '{}' {}",
                    a.label(),
                    if ok { "OK - sha256 verified on host" } else { "FAILED host verification" }
                );
                finish_oneshot(status, ok);
            }
        }
        "error" => {
            let code = jstr(&json, "code").unwrap_or_default();
            let msg = jstr(&json, "msg").unwrap_or_default();
            if let Some(a) = transfers.remove(&id) {
                println!("  [xfer] FAILED '{}': [{}] {}", a.label(), code, msg);
                finish_oneshot(status, false);
            }
        }
        _ => {}
    }
}

fn finalize_get(
    ws: &mut WebSocket<TcpStream>,
    transfers: &mut HashMap<u32, Active>,
    status: &Option<Arc<AtomicU8>>,
    id: u32,
) {
    let Some(Active::Get { label, mut tx }) = transfers.remove(&id) else {
        return;
    };
    let hasher = std::mem::replace(&mut tx.hasher, Sha256::new());
    let digest = hex(&hasher.finalize());
    let ok = digest.eq_ignore_ascii_case(&tx.expect_sha);
    if ok {
        println!("  [xfer] GET '{}' OK ({} bytes, sha256 verified)", label, tx.written);
    } else {
        println!(
            "  [xfer] GET '{}' SHA256 MISMATCH expected={} got={} (partial kept for resume)",
            label, tx.expect_sha, digest
        );
    }
    let _ = send_meta_json(ws, format!("{{\"t\":\"done-ack\",\"id\":{}}}", id));
    finish_oneshot(status, ok);
}

/// client<-host data chunks for an active GET
fn handle_chunk(
    payload: &[u8],
    ws: &mut WebSocket<TcpStream>,
    transfers: &mut HashMap<u32, Active>,
    status: &Option<Arc<AtomicU8>>,
) {
    if payload.len() < 12 {
        return;
    }
    let id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let offset = u64::from_be_bytes([
        payload[4], payload[5], payload[6], payload[7], payload[8], payload[9], payload[10], payload[11],
    ]);
    let data = &payload[12..];
    let Some(Active::Get { tx, .. }) = transfers.get_mut(&id) else {
        return; // unknown/stale chunk: drop silently
    };
    if offset != tx.written {
        return; // out of order: drop
    }
    let Some(file) = tx.file.as_mut() else { return };
    if file.write_all(data).is_err() {
        println!("  [xfer] write failed mid-download");
        transfers.remove(&id);
        finish_oneshot(status, false);
        return;
    }
    tx.hasher.update(data);
    tx.written += data.len() as u64;
    let pct = if tx.size > 0 { ((tx.written.min(tx.size)) * 100 / tx.size) as i64 } else { 100 };
    if pct >= tx.last_pct + 10 || tx.written >= tx.size {
        tx.last_pct = pct;
        println!("  [xfer] GET #{} {}%", id, pct);
    }
    if tx.size > 0 && tx.written >= tx.size {
        finalize_get(ws, transfers, status, id);
    }
}

/// Root channel: answers host pings, streams remote-command output, and in
/// interactive mode feeds typed lines to the host's terminal channel.
fn post_auth_loop(
    stream: &mut TcpStream,
    interactive: bool,
    dl_state: &AtomicU8,
    xfer_tx: &mpsc::Sender<DlCommand>,
    xfer_status: &AtomicU8,
    mode: &RunMode,
) -> Result<(), String> {
    let stdin_rx = if interactive { Some(spawn_stdin_reader()) } else { None };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut last_ping_seen = Instant::now();
    let mut awaiting_result = false;
    loop {
        if let Some(rx) = &stdin_rx {
            while let Ok(m) = rx.try_recv() {
                match m {
                    StdinMsg::Eof => {
                        if interactive {
                            println!();
                        }
                        let _ = proto::write_frame(stream, BYE, b"");
                        return Ok(());
                    }
                    StdinMsg::Line(l) => {
                        let l = l.trim();
                        if l.is_empty() {
                            continue;
                        }
                        if l == "exit" || l == "quit" {
                            let _ = proto::write_frame(stream, BYE, b"");
                            println!("  [bye] closing session.");
                            return Ok(());
                        }
                        // file-transfer verbs ride the ws data link
                        if l == "get" || l.starts_with("get ") {
                            handle_xfer_verb(l, xfer_tx);
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if l == "put" || l.starts_with("put ") {
                            handle_xfer_verb(l, xfer_tx);
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if awaiting_result {
                            println!("  [i] a command is still running — wait for it to finish");
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        // 'sudo <cmd>' requests elevation on the host side
                        let (elevated, cmd) = match l.strip_prefix("sudo ") {
                            Some(rest) if !rest.trim().is_empty() => (true, rest.trim()),
                            Some(_) => {
                                println!("  [i] usage: sudo <command>");
                                print!("pyielink> ");
                                let _ = std::io::stdout().flush();
                                continue;
                            }
                            None => (false, l),
                        };
                        let mut payload = Vec::with_capacity(cmd.len() + 1);
                        payload.push(if elevated { b'1' } else { b'0' });
                        payload.extend_from_slice(cmd.as_bytes());
                        if proto::write_frame(stream, EXEC_REQ, &payload).is_err() {
                            return Err("connection lost".into());
                        }
                        awaiting_result = true;
                    }
                }
            }
        }
        match proto::read_frame(stream) {
            Ok((PING, payload)) => {
                last_ping_seen = Instant::now();
                if let Ok(sent) = String::from_utf8_lossy(&payload).trim().parse::<u128>() {
                    println!("  [hb] rtt {}ms", now_ms().saturating_sub(sent));
                } else {
                    println!("  [hb] alive");
                }
                if proto::write_frame(stream, PONG, &payload).is_err() {
                    return Err("lost connection to host".into());
                }
            }
            Ok((BYE, _)) => {
                println!("  [bye] host closed the session");
                return Ok(());
            }
            Ok((EXEC_OUT, chunk)) => {
                let mut out = std::io::stdout();
                let _ = out.write_all(&chunk);
                let _ = out.flush();
            }
            Ok((EXEC_END, code)) => {
                awaiting_result = false;
                println!("\n  [exit {}]", String::from_utf8_lossy(&code).trim());
                if interactive {
                    print!("pyielink> ");
                    let _ = std::io::stdout().flush();
                }
            }
            Ok((EXEC_DENY, reason)) => {
                awaiting_result = false;
                println!("\n  [denied] {}", String::from_utf8_lossy(&reason).trim());
                if interactive {
                    print!("pyielink> ");
                    let _ = std::io::stdout().flush();
                }
            }
            Ok((PONG, _)) => {
                last_ping_seen = Instant::now();
            }
            Ok(_) => {}
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if dl_state.load(Ordering::Relaxed) == DL_DEAD {
                    let _ = proto::write_frame(stream, BYE, b"data-link lost");
                    return Err("data link died — session ended".into());
                }
                // one-shot transfer modes: leave once the outcome lands
                if !matches!(mode, RunMode::Shell) && xfer_status.load(Ordering::Relaxed) != XFER_RUN {
                    let _ = proto::write_frame(stream, BYE, b"transfer done");
                    return Ok(());
                }
                if last_ping_seen.elapsed() >= HB_STALE {
                    let _ = proto::write_frame(stream, BYE, b"stall");
                    return Err("host stopped responding to heartbeats".into());
                }
            }
            Err(_) => return Err("connection lost".into()),
        }
    }
}

fn expect_frame(stream: &mut TcpStream) -> Result<(u8, Vec<u8>), String> {
    proto::read_frame(stream).map_err(|e| format!("connection lost: {}", e))
}

fn confirm_license() -> bool {
    loop {
        print!("\naccept agreement? [y/n]: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return true,
            "n" | "no" => return false,
            _ => println!("please answer 'y' or 'n'."),
        }
    }
}

/// Best-effort local address discovery for status lines (no routing tables needed).
#[allow(dead_code)]
fn local_hint(ip: &str) -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((ip, BOOTSTRAP_PORT)).ok()?;
    s.local_addr().ok().map(|a| a.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_parsing() {
        let key = "a".repeat(64);
        assert!(split_ticket(&format!("4243\n{}", key)).is_ok());
        assert!(split_ticket(&format!("4243\n{}", key)).unwrap().0 == "4243");
        assert!(split_ticket("4243").is_err());
        assert!(split_ticket("4243\nshort").is_err());
        assert!(split_ticket("\nsomekey").is_err());
    }

    #[test]
    fn target_parsing() {
        assert!(parse_target("bob@127.0.0.1").is_ok());
        assert!(parse_target("@127.0.0.1").is_err());
        assert!(parse_target("bob@").is_err());
        assert!(parse_target("bob-127.0.0.1").is_err());
    }
}
