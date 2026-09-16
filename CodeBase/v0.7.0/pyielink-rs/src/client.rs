use crate::creds;
use crate::proto::{self, *};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::{Message, WebSocket};

const BOOTSTRAP_PORT: u16 = 4242;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HB_STALE: Duration = Duration::from_secs(20);
/// data-link lifecycle: 0 = connecting/authenticating, 1 = up, 2 = dead, 3 = reconnecting
const DL_CONNECTING: u8 = 0;
const DL_UP: u8 = 1;
const DL_DEAD: u8 = 2;
const DL_RECONNECTING: u8 = 3;
const DL_POLL: Duration = Duration::from_millis(200);
const DL_STALE: Duration = Duration::from_secs(15);
/// file-transfer channels (mirrors datalayer/src/mux.js CHANNELS)
const DL_CH_META: u8 = 0x04;
const DL_CH_CHUNK: u8 = 0x05;
/// input channel (mirrors CHANNELS.INPUT = 0x02)
const DL_CH_INPUT: u8 = 0x02;
/// chunk size + how many chunks we push per service-loop pass so dl
/// heartbeats keep flowing while an upload drains (backpressure is the
/// blocking TCP write; pacing keeps the control loop alive)
const XFER_CHUNK: usize = 64 * 1024;
const XFER_PUMP_CHUNKS: usize = 8;
/// xfer_status values: 0 running/none, 1 ok, 2 failed
const XFER_RUN: u8 = 0;
const XFER_OK: u8 = 1;
const XFER_FAIL: u8 = 2;
/// max reconnection attempts
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
/// delay between reconnection attempts
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct SessionState {
    pub user: String,
    pub ip: String,
    pub session_key: String,
    pub data_port: String,
    pub input_running: bool,
    pub transfers: Vec<TransferState>,
}

#[derive(Clone, Debug)]
pub struct TransferState {
    pub id: u32,
    pub label: String,
    pub is_get: bool,
    pub remote: String,
    pub local: PathBuf,
    pub offset: u64,
    pub size: u64,
    pub expect_sha: String,
    pub written: u64,
    pub hasher_state: Vec<u8>, // serialized hasher state
}

impl SessionState {
    pub fn new(user: String, ip: String, session_key: String, data_port: String) -> Self {
        Self {
            user,
            ip,
            session_key,
            data_port,
            input_running: false,
            transfers: Vec::new(),
        }
    }
}

pub enum DlCommand {
    Get { remote: String, local: PathBuf },
    Put { local: PathBuf, remote: String },
    Input { events: Vec<InputEvent> },
    InputStart,
    InputStop,
    /// list a remote directory (`readdir` on the FILE_META channel)
    ListDir { path: String },
    CreateDir { path: String },
    Delete { path: String, recursive: bool },
    Rename { old: String, new: String },
    /// server-side copy (files.js `copy` op) — used for remote-to-remote
    /// copy/paste without pulling the bytes through the client
    Copy { from: String, to: String },
}

/// One entry in a remote directory listing.
#[derive(Debug, Clone, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// modification time in epoch milliseconds (host-reported)
    pub mtime: Option<u64>,
}

/// Result of a remote `readdir` request.
#[derive(Debug, Clone, PartialEq)]
pub struct DirListing {
    pub path: String,
    pub entries: Vec<DirEntry>,
}

/// Async results delivered from the data-link thread back to the REPL (and,
/// via the GUI session API, the file-explorer app).
#[derive(Debug, Clone)]
pub enum RemoteEvent {
    DirListing(DirListing),
    OpResult { op: String, ok: bool, msg: String },
    /// a get/put transfer finished (label + overall ok)
    TransferDone { label: String, ok: bool },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum InputEvent {
    KeyDown { vk: u16, scan: u16, flags: u32 },
    KeyUp { vk: u16, scan: u16, flags: u32 },
    MouseMove { x: i32, y: i32, flags: u32 },
    MouseDown { button: u16, x: i32, y: i32, flags: u32 },
    MouseUp { button: u16, x: i32, y: i32, flags: u32 },
    MouseWheel { delta: i32, x: i32, y: i32, flags: u32 },
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

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

pub fn run_connect(target: &str, _repl_mode: bool) -> Result<(), String> {
    // Single client mode: the remote terminal + file-explorer REPL.
    // The `--repl` flag and bare `user@ip` behave identically; the file-explorer
    // GUI is launched separately via the `gui` module (feature-gated).
    run_session(target, RunMode::Shell)
}

/// ---- File-explorer GUI session API (feature-gated in lib.rs) ----

/// Commands a connected file-explorer session can issue.
#[derive(Debug, Clone)]
pub enum GuiCommand {
    Exec { cmd: String, elevated: bool },
    ListDir { path: String },
    CreateDir { path: String },
    Delete { path: String, recursive: bool },
    Rename { old: String, new: String },
    Copy { from: String, to: String },
    Get { remote: String, local: PathBuf },
    Put { local: PathBuf, remote: String },
}

/// Events a file-explorer session emits back to the GUI.
#[derive(Debug, Clone)]
pub enum GuiEvent {
    Ready,
    DirListing(DirListing),
    OpResult { op: String, ok: bool, msg: String },
    TransferDone { label: String, ok: bool },
    ExecOutput(Vec<u8>),
    ExecEnd { code: String, denied: bool },
    Message(String),
    Closed(String),
}

pub struct GuiSession {
    cmd_tx: mpsc::Sender<GuiCommand>,
    events_rx: mpsc::Receiver<GuiEvent>,
    stop: Arc<AtomicU8>,
    _handle: Option<std::thread::JoinHandle<()>>,
}

impl GuiSession {
    /// Connect as `user@ip` using `password`, spawn the data link, and run the
    /// GUI command/event loop on a background thread.
    pub fn connect(
        target: &str,
        password: &str,
        accept_license: bool,
    ) -> Result<GuiSession, String> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<GuiCommand>();
        let (events_tx, events_rx) = mpsc::channel::<GuiEvent>();
        let stop = Arc::new(AtomicU8::new(0));
        let stop2 = Arc::clone(&stop);
        let target = target.to_string();
        let password = password.to_string();
        let handle = std::thread::spawn(move || {
            let result = gui_session_run(
                &target,
                &password,
                accept_license,
                cmd_rx,
                events_tx.clone(),
                stop2,
            );
            let _ = events_tx.send(GuiEvent::Closed(match result {
                Ok(()) => String::new(),
                Err(e) => e,
            }));
        });
        Ok(GuiSession {
            cmd_tx,
            events_rx,
            stop,
            _handle: Some(handle),
        })
    }

    pub fn send(&self, cmd: GuiCommand) -> Result<(), String> {
        self.cmd_tx.send(cmd).map_err(|_| "session closed".to_string())
    }

    /// Non-blocking event poll for the GUI frame loop.
    pub fn try_event(&self) -> Option<GuiEvent> {
        self.events_rx.try_recv().ok()
    }

    pub fn close(&self) {
        self.stop.store(1, Ordering::Relaxed);
    }
}

fn gui_session_run(
    target: &str,
    password: &str,
    accept_license: bool,
    cmd_rx: mpsc::Receiver<GuiCommand>,
    events: mpsc::Sender<GuiEvent>,
    stop: Arc<AtomicU8>,
) -> Result<(), String> {
    use std::net::UdpSocket;
    let (user, ip) = parse_target(target)?;
    let port: u16 = std::env::var("PYIELINK_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(BOOTSTRAP_PORT);
    let addr = format!("{}:{}", ip, port);
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| format!("cannot reach {} — is the host running? ({})", addr, e))?;
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|e| e.to_string())?;
    if password.is_empty() {
        return Err("a password is required to connect".into());
    }
    let (data_port, session_key) =
        do_handshake(&mut stream, &user, &ip, Some(password), accept_license, false)?;

    let dl_state = Arc::new(AtomicU8::new(DL_CONNECTING));
    let dl_stop = Arc::new(AtomicU8::new(0));
    let xfer_status = Arc::new(AtomicU8::new(XFER_RUN));
    let (xfer_tx, xfer_rx) = mpsc::channel::<DlCommand>();
    let (remote_events_tx, remote_events_rx) = mpsc::channel::<RemoteEvent>();
    let session_state = Arc::new(std::sync::Mutex::new(SessionState::new(
        user.clone(),
        ip.clone(),
        session_key.clone(),
        data_port.clone(),
    )));
    {
        let ip = ip.clone();
        let dp = data_port.clone();
        let sk = session_key.clone();
        let ds = Arc::clone(&dl_state);
        let ds2 = Arc::clone(&dl_stop);
        let xs = Arc::clone(&xfer_status);
        let ss = Arc::clone(&session_state);
        std::thread::spawn(move || {
            data_link_with_reconnect(
                &ip, &dp, &sk, ds, ds2, xfer_rx, Some(xs), Some(remote_events_tx), ss,
            );
        });
    }

    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut last_ping_seen = Instant::now();
    let _ = events.send(GuiEvent::Ready);

    loop {
        if stop.load(Ordering::Relaxed) == 1 {
            let _ = proto::write_frame(&mut stream, BYE, b"gui close");
            return Ok(());
        }
        while let Ok(ev) = remote_events_rx.try_recv() {
            let ge = match ev {
                RemoteEvent::DirListing(l) => GuiEvent::DirListing(l),
                RemoteEvent::OpResult { op, ok, msg } => GuiEvent::OpResult { op, ok, msg },
                RemoteEvent::TransferDone { label, ok } => GuiEvent::TransferDone { label, ok },
            };
            let _ = events.send(ge);
        }
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                GuiCommand::Exec { cmd, elevated } => {
                    if cmd.is_empty() {
                        continue;
                    }
                    let mut payload = Vec::with_capacity(cmd.len() + 1);
                    payload.push(if elevated { b'1' } else { b'0' });
                    payload.extend_from_slice(cmd.as_bytes());
                    let _ = proto::write_frame(&mut stream, EXEC_REQ, &payload);
                }
                GuiCommand::ListDir { path } => {
                    let _ = xfer_tx.send(DlCommand::ListDir { path });
                }
                GuiCommand::CreateDir { path } => {
                    let _ = xfer_tx.send(DlCommand::CreateDir { path });
                }
                GuiCommand::Delete { path, recursive } => {
                    let _ = xfer_tx.send(DlCommand::Delete { path, recursive });
                }
                GuiCommand::Rename { old, new } => {
                    let _ = xfer_tx.send(DlCommand::Rename { old, new });
                }
                GuiCommand::Copy { from, to } => {
                    let _ = xfer_tx.send(DlCommand::Copy { from, to });
                }
                GuiCommand::Get { remote, local } => {
                    let _ = xfer_tx.send(DlCommand::Get { remote, local });
                }
                GuiCommand::Put { local, remote } => {
                    let _ = xfer_tx.send(DlCommand::Put { local, remote });
                }
            }
        }
        match proto::read_frame(&mut stream) {
            Ok((PING, payload)) => {
                let _ = proto::write_frame(&mut stream, PONG, &payload);
                last_ping_seen = Instant::now();
            }
            Ok((EXEC_OUT, chunk)) => {
                let _ = events.send(GuiEvent::ExecOutput(chunk));
            }
            Ok((EXEC_END, code)) => {
                let _ = events.send(GuiEvent::ExecEnd {
                    code: String::from_utf8_lossy(&code).into_owned(),
                    denied: false,
                });
            }
            Ok((EXEC_DENY, reason)) => {
                let _ = events.send(GuiEvent::ExecEnd {
                    code: String::from_utf8_lossy(&reason).into_owned(),
                    denied: true,
                });
            }
            Ok((BYE, _)) => {
                let _ = events.send(GuiEvent::Closed("host closed".into()));
                return Ok(());
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
                    let _ = proto::write_frame(&mut stream, BYE, b"data-link lost");
                    return Err("data link died".into());
                }
                if last_ping_seen.elapsed() >= HB_STALE {
                    let _ = proto::write_frame(&mut stream, BYE, b"stall");
                    return Err("host stopped responding to heartbeats".into());
                }
            }
            Err(_) => {
                return Err("connection lost".into());
            }
        }
    }
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

/// Basename without the final extension (for " (copy)" suffixing in the GUI).
pub fn stem_of(p: &str) -> &str {
    let base = basename_of(p);
    match base.rfind('.') {
        Some(idx) if idx > 0 => &base[..idx],
        _ => base,
    }
}

pub fn run_session(target: &str, mode: RunMode) -> Result<(), String> {
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
    let mut attempts = 0u32;
    let data_port;
    let session_key;
    
    loop {
        match expect_frame(&mut stream)? {
            (CHALLENGE, payload) => {
                let line = String::from_utf8_lossy(&payload).into_owned();
                let (salt, nonce) = match line.split_once('\n') {
                    Some(x) => (x.0.to_string(), x.1.trim().to_string()),
                    None => return Err("malformed challenge from host".into()),
                };
                // Password is required on every connection by default;
                // scripted runs must never block on a hidden console
                // read: fail fast instead of hang.
                if !creds::stdin_is_tty()
                    && std::env::var("PYIELINK_SHELL").as_deref() != Ok("1")
                    && std::env::var("PYIELINK_REPL").as_deref() != Ok("1")
                    && !matches!(mode, RunMode::Shell)
                {
                    return Err(format!(
                        "password required for {}@{} — run from an interactive terminal (or set PYIELINK_SHELL=1 for scripted use)",
                        user, ip
                    ));
                }
                if attempts >= 3 {
                    return Err("too many failed authentication attempts".into());
                }
                attempts += 1;
                let framed = format!("p:{}", password_proof(&salt, &nonce));
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
            (TOKEN_ISSUED, _payload) => {
                // Not stored: password is required each connection.
            }
            (AUTH_OK, payload) => {
                let ticket = String::from_utf8_lossy(&payload).into_owned();
                let (dp, sk) = split_ticket(ticket.trim())?;
                data_port = dp;
                session_key = sk;
                println!(
                    "  [ok] session promoted. data layer ready on {}:{}. session key received.",
                    ip, data_port
                );
                break;
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
    
    // Initialize session state
    let session_state_arc = Arc::new(std::sync::Mutex::new(SessionState::new(user.clone(), ip.clone(), session_key.clone(), data_port.clone())));
    let dl_state = Arc::new(AtomicU8::new(DL_CONNECTING));
    let dl_stop = Arc::new(AtomicU8::new(0));
    let xfer_status = Arc::new(AtomicU8::new(XFER_RUN));
    let (xfer_tx, xfer_rx) = mpsc::channel::<DlCommand>();
    let (events_tx, events_rx) = mpsc::channel::<RemoteEvent>();
    
    // Spawn data link thread with reconnection logic
    let (ip_clone, data_port_clone, session_key_clone) = (ip.clone(), data_port.clone(), session_key.clone());
    let dl_state_clone = Arc::clone(&dl_state);
    let dl_stop_clone = Arc::clone(&dl_stop);
    let xfer_status_clone = Arc::clone(&xfer_status);
    let session_state_arc_clone = Arc::clone(&session_state_arc);
    let xfer_rx_clone = xfer_rx;
    let events_tx_clone = events_tx;
    
    let dl_handle = std::thread::spawn(move || {
        data_link_with_reconnect(
            &ip_clone,
            &data_port_clone,
            &session_key_clone,
            dl_state_clone,
            dl_stop_clone,
            xfer_rx_clone,
            Some(xfer_status_clone),
            Some(events_tx_clone),
            session_state_arc_clone,
        )
    });
    
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
        || std::env::var("PYIELINK_REPL").ok().as_deref() == Some("1")
        || matches!(mode, RunMode::Shell)
        || creds::stdin_is_tty();
    let input_running = Arc::new(AtomicBool::new(false));
    let mut input_handle: Option<std::thread::JoinHandle<()>> = None;
    if interactive && matches!(mode, RunMode::Shell) {
        println!("  [i] remote terminal ready — ls/cd/pwd/mkdir/rm/mv, get/put, input start/stop, exit");
        print!("pyielink> ");
        let _ = std::io::stdout().flush();
    }
    let outcome =
        post_auth_loop(&mut stream, interactive, &dl_state, &xfer_tx, &xfer_status, &mode, &input_running, &mut input_handle, &events_rx);
    dl_stop.store(1, Ordering::Relaxed);
    let _ = dl_handle.join();
    outcome?;
    if matches!(mode, RunMode::Shell) || xfer_status.load(Ordering::Relaxed) == XFER_OK {
        return Ok(());
    }
    return Err("file transfer failed".into());
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

/// Full bootstrap handshake: hello → challenge → proof → license → promotion.
/// Returns `(data_port, session_key)`.
fn do_handshake(
    stream: &mut TcpStream,
    user: &str,
    ip: &str,
    password: Option<&str>,
    accept_license: bool,
    silent_no_tty: bool,
) -> Result<(String, String), String> {
    let mut attempts = 0u32;
    let hello = format!("{}\n{}\n", user, env!("CARGO_PKG_VERSION"));
    proto::write_frame(stream, HELLO, hello.as_bytes())
        .map_err(|e| format!("handshake send failed: {}", e))?;

    loop {
        match expect_frame(stream)? {
            (CHALLENGE, payload) => {
                let line = String::from_utf8_lossy(&payload).into_owned();
                let (salt, nonce) = match line.split_once('\n') {
                    Some(x) => (x.0.to_string(), x.1.trim().to_string()),
                    None => return Err("malformed challenge from host".into()),
                };
                if silent_no_tty && !creds::stdin_is_tty() {
                    return Err(format!(
                        "password required for {}@{} — run from an interactive terminal (or set PYIELINK_SHELL=1 for scripted use)",
                        user, ip
                    ));
                }
                if attempts >= 3 {
                    return Err("too many failed authentication attempts".into());
                }
                attempts += 1;
                let pw = match password {
                    Some(p) => p.to_string(),
                    None => prompt_password(),
                };
                let framed = format!(
                    "p:{}",
                    creds::compute_proof(&creds::hash_password(&salt, &pw), &nonce)
                );
                proto::write_frame(stream, PROOF, framed.as_bytes())
                    .map_err(|e| format!("send failed: {}", e))?;
            }
            (LICENSE_TEXT, payload) => {
                if accept_license || license_preaccepted() {
                    if license_preaccepted() {
                        println!(
                            "  [i] agreement pre-accepted via PYIELINK_ACCEPT_LICENSE \
                             (you are accountable for authorization)"
                        );
                    }
                } else {
                    println!("\n{}", String::from_utf8_lossy(&payload));
                    if !confirm_license() {
                        proto::write_frame(stream, LICENSE_REJECT, b"n")
                            .map_err(|e| e.to_string())?;
                        return Err("license rejected — session aborted".into());
                    }
                }
                proto::write_frame(stream, LICENSE_ACCEPT, b"y")
                    .map_err(|e| e.to_string())?;
            }
            (TOKEN_ISSUED, _) => { /* not stored — password required each time */ }
            (AUTH_OK, payload) => {
                let ticket = String::from_utf8_lossy(&payload).into_owned();
                let (dp, sk) = split_ticket(ticket.trim())?;
                println!(
                    "  [ok] session promoted. data layer ready on {}:{}. session key received.",
                    ip, dp
                );
                return Ok((dp, sk));
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

#[derive(Debug, PartialEq)]
enum DlLinkResult {
    Stopped,      // Clean stop via stop flag
    Reconnect,    // Connection lost, should reconnect
}

/// Single data-link connection attempt. Returns DlLinkResult indicating
/// whether to reconnect or stop.
#[allow(clippy::too_many_arguments)]
fn data_link_connect(
    ip: &str,
    port: &str,
    key: &str,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicU8>,
    cmds: &mpsc::Receiver<DlCommand>,
    events: Option<mpsc::Sender<RemoteEvent>>,
    status: Option<Arc<AtomicU8>>,
    session_state: &Arc<std::sync::Mutex<SessionState>>,
) -> DlLinkResult {
    let dl_host = std::env::var("PYIELINK_DL_HOST").unwrap_or_else(|_| ip.to_string());
    let dl_port = std::env::var("PYIELINK_DL_PORT").unwrap_or_else(|_| port.to_string());
    // A tunnel may return a full public address (host:port) as the data port;
    // connect to it directly instead of <dl_host>:<dl_port>.
    let addr = if dl_port.contains(':') {
        dl_port.clone()
    } else {
        format!("{}:{}", dl_host, dl_port)
    };
    let tcp = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  [dl-link] cannot reach {}: {}", addr, e);
            state.store(DL_DEAD, Ordering::Relaxed);
            return DlLinkResult::Reconnect;
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
            return DlLinkResult::Reconnect;
        }
    };

    // first message must be the session key; server answers a plain JSON ack
    if ws.send(Message::Text(format!("{{\"k\":\"{}\"}}", key))).is_err() {
        eprintln!("  [dl-link] failed to send session key");
        state.store(DL_DEAD, Ordering::Relaxed);
        return DlLinkResult::Reconnect;
    }
    loop {
        if stop.load(Ordering::Relaxed) == 1 {
            dl_shutdown(&mut ws);
            return DlLinkResult::Stopped;
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                if t.contains("\"ok\":true") || t.contains("\"ok\": true") {
                    println!("  [dl-link] data channel up ({})", t.trim());
                    break;
                }
                eprintln!("  [dl-link] unexpected ack: {}", t.trim());
                state.store(DL_DEAD, Ordering::Relaxed);
                return DlLinkResult::Reconnect;
            }
            Ok(Message::Close(c)) => {
                eprintln!(
                    "  [dl-link] rejected by data layer (code {})",
                    c.map(|f| u16::from(f.code)).unwrap_or(0)
                );
                state.store(DL_DEAD, Ordering::Relaxed);
                return DlLinkResult::Reconnect;
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
                return DlLinkResult::Reconnect;
            }
        }
    }
    state.store(DL_UP, Ordering::Relaxed);

    // control-channel heartbeat: answer server PINGs, send our own, log RTT
    // file-transfer state (Phase 3.1)
    let mut transfers: HashMap<u32, Active> = HashMap::new();
    let mut next_id: u32 = 1;
    
    // Restore transfer state from session_state if available
    {
        let state_guard = session_state.lock().unwrap();
        for ts in &state_guard.transfers {
            if ts.is_get {
                transfers.insert(ts.id, Active::Get {
                    label: ts.label.clone(),
                    tx: ActiveGet {
                        local: ts.local.clone(),
                        file: None,
                        written: ts.written,
                        size: ts.size,
                        expect_sha: ts.expect_sha.clone(),
                        hasher: Sha256::new(), // will be seeded on first chunk
                        seeded: ts.written == 0,
                        last_pct: -1,
                    },
                });
            } else {
                transfers.insert(ts.id, Active::Put {
                    label: ts.label.clone(),
                    tx: ActivePut {
                        path: ts.local.clone(),
                        reader: None,
                        next_offset: ts.offset,
                        size: ts.size,
                        awaiting_done: false,
                        last_pct: 0,
                    },
                });
            }
            next_id = next_id.max(ts.id + 1);
        }
    }

    // If input was running, restart it
    {
        let state_guard = session_state.lock().unwrap();
        if state_guard.input_running {
            let json = r#"{"t":"input_start"}"#;
            let _ = ws.send(Message::Binary(dl_frame(DL_CH_INPUT, json.as_bytes())));
        }
    }

    // control-channel heartbeat: answer server PINGs, send our own, log RTT
    let mut last_seen = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) == 1 {
            // Save transfer state before shutdown
            save_session_state(session_state, &transfers);
            dl_shutdown(&mut ws);
            return DlLinkResult::Stopped;
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
                Some((0x01, payload)) => {
                    // Server drives the heartbeat ("PING"): answer "PONG".
                    // Anything else just refreshes liveness.
                    last_seen = Instant::now();
                    if payload == b"PING" || payload == b"P" {
                        let _ = ws.send(Message::Binary(dl_frame(0x01, b"PONG")));
                    }
                }
                Some((DL_CH_META, payload)) => handle_meta_json(payload, &mut ws, &mut transfers, &status, &events),
                Some((DL_CH_CHUNK, payload)) => handle_chunk(payload, &mut ws, &mut transfers, &status, &events),
                _ => {} // unknown channel: drop silently (mirrors mux policy)
            },
            Ok(Message::Close(_)) | Ok(Message::Frame(_)) => {
                println!("  [dl-link] data layer closed the channel");
                state.store(DL_DEAD, Ordering::Relaxed);
                save_session_state(session_state, &transfers);
                return DlLinkResult::Reconnect;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                if last_seen.elapsed() >= DL_STALE {
                    eprintln!("  [dl-link] data layer stopped responding — tearing down");
                    state.store(DL_DEAD, Ordering::Relaxed);
                    save_session_state(session_state, &transfers);
                    return DlLinkResult::Reconnect;
                }
                // The data layer drives heartbeats; the client only answers
                // PINGs (above). No client-initiated pings: they used to be
                // echoed back by the server and created an infinite loop.
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
                save_session_state(session_state, &transfers);
                return DlLinkResult::Reconnect;
            }
        }
    }
}

fn save_session_state(session_state: &Arc<std::sync::Mutex<SessionState>>, transfers: &HashMap<u32, Active>) {
    let mut state_guard = session_state.lock().unwrap();
    state_guard.transfers.clear();
    for (id, active) in transfers {
        match active {
            Active::Get { label, tx } => {
                state_guard.transfers.push(TransferState {
                    id: *id,
                    label: label.clone(),
                    is_get: true,
                    remote: label.split(" -> ").next().unwrap_or("").to_string(),
                    local: tx.local.clone(),
                    offset: tx.written,
                    size: tx.size,
                    expect_sha: tx.expect_sha.clone(),
                    written: tx.written,
                    hasher_state: Vec::new(), // would need custom serialization
                });
            }
            Active::Put { label, tx } => {
                state_guard.transfers.push(TransferState {
                    id: *id,
                    label: label.clone(),
                    is_get: false,
                    remote: label.split(" -> ").nth(1).unwrap_or("").to_string(),
                    local: tx.path.clone(),
                    offset: tx.next_offset,
                    size: tx.size,
                    expect_sha: String::new(),
                    written: tx.next_offset,
                    hasher_state: Vec::new(),
                });
            }
        }
    }
}

fn dl_shutdown(ws: &mut WebSocket<TcpStream>) {
    let _ = ws.close(None);
    let _ = ws.flush();
}

/// Data link with reconnection logic - runs in a background thread.
/// Handles connection, reconnection, and session state persistence.
fn data_link_with_reconnect(
    ip: &str,
    port: &str,
    key: &str,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicU8>,
    cmds: mpsc::Receiver<DlCommand>,
    status: Option<Arc<AtomicU8>>,
    events: Option<mpsc::Sender<RemoteEvent>>,
    session_state: Arc<std::sync::Mutex<SessionState>>,
) {
    let mut reconnect_attempts = 0u32;
    loop {
        if stop.load(Ordering::Relaxed) == 1 {
            return;
        }
        let result = data_link_connect(
            ip,
            port,
            key,
            Arc::clone(&state),
            Arc::clone(&stop),
            &cmds,
            events.clone(),
            status.clone(),
            &session_state,
        );
        
        match result {
            DlLinkResult::Stopped => {
                return;
            }
            DlLinkResult::Reconnect => {
                reconnect_attempts += 1;
                if reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
                    eprintln!("  [dl-link] max reconnection attempts ({}) reached, giving up", MAX_RECONNECT_ATTEMPTS);
                    state.store(DL_DEAD, Ordering::Relaxed);
                    return;
                }
                println!("  [reconnect] attempt {}/{} in {:?}...", reconnect_attempts, MAX_RECONNECT_ATTEMPTS, RECONNECT_DELAY);
                state.store(DL_RECONNECTING, Ordering::Relaxed);
                std::thread::sleep(RECONNECT_DELAY);
                
                // Re-authenticate to get new session key
                // The outer run_session loop will handle re-authentication
                // and call data_link_with_reconnect again with new credentials
                return; // Exit to let run_session re-authenticate
            }
        }
    }
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

fn handle_input_verb(
    line: &str,
    tx: &mpsc::Sender<DlCommand>,
    input_running: &Arc<AtomicBool>,
    input_handle: &mut Option<std::thread::JoinHandle<()>>,
) {
    let mut it = line.split_whitespace();
    let verb = it.next().unwrap_or("");
    let arg = it.next().unwrap_or("");
    match (verb, arg) {
        ("input", "start") => {
            if input_running.load(Ordering::Relaxed) {
                println!("  [input] already running");
            } else {
                input_running.store(true, Ordering::Relaxed);
                if tx.send(DlCommand::InputStart).is_err() {
                    println!("  [input] data link not available");
                } else {
                    println!("  [input] capture started");
                }
                *input_handle = Some(crate::input::start_input_capture(input_running.clone(), tx.clone()));
            }
        }
        ("input", "stop") => {
            if !input_running.load(Ordering::Relaxed) {
                println!("  [input] not running");
            } else {
                input_running.store(false, Ordering::Relaxed);
                if tx.send(DlCommand::InputStop).is_err() {
                    println!("  [input] data link not available");
                } else {
                    println!("  [input] capture stopped");
                }
                if let Some(h) = input_handle.take() {
                    let _ = h.join();
                }
            }
        }
        _ => {
            println!("  [i] usage: input start | input stop");
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
        DlCommand::Input { events } => {
            let json = serde_json::to_string(&events)
                .map_err(|e| format!("serialize input events: {}", e))?;
            ws.send(Message::Binary(dl_frame(DL_CH_INPUT, json.as_bytes())))
                .map_err(|e| format!("send input events: {}", e))?;
            Ok(())
        }
        DlCommand::InputStart => {
            let json = r#"{"t":"input_start"}"#;
            ws.send(Message::Binary(dl_frame(DL_CH_INPUT, json.as_bytes())))
                .map_err(|e| format!("send input start: {}", e))?;
            println!("  [input] capture started");
            Ok(())
        }
        DlCommand::InputStop => {
            let json = r#"{"t":"input_stop"}"#;
            ws.send(Message::Binary(dl_frame(DL_CH_INPUT, json.as_bytes())))
                .map_err(|e| format!("send input stop: {}", e))?;
            println!("  [input] capture stopped");
            Ok(())
        }
        DlCommand::ListDir { path } => send_meta_json(
            ws,
            format!("{{\"t\":\"readdir\",\"id\":{},\"path\":\"{}\"}}", *next_id, json_escape(&path)),
        )
        .map(|_| {
            *next_id += 1;
        }),
        DlCommand::CreateDir { path } => {
            let id = *next_id;
            *next_id += 1;
            send_meta_json(ws, format!("{{\"t\":\"mkdir\",\"id\":{},\"path\":\"{}\"}}", id, json_escape(&path)))
        }
        DlCommand::Delete { path, recursive } => {
            let id = *next_id;
            *next_id += 1;
            send_meta_json(
                ws,
                format!(
                    "{{\"t\":\"delete\",\"id\":{},\"path\":\"{}\",\"recursive\":{}}}",
                    id,
                    json_escape(&path),
                    recursive
                ),
            )
        }
        DlCommand::Rename { old, new } => {
            let id = *next_id;
            *next_id += 1;
            send_meta_json(
                ws,
                format!(
                    "{{\"t\":\"rename\",\"id\":{},\"old\":\"{}\",\"new\":\"{}\"}}",
                    id,
                    json_escape(&old),
                    json_escape(&new)
                ),
            )
        }
        DlCommand::Copy { from, to } => {
            let id = *next_id;
            *next_id += 1;
            send_meta_json(
                ws,
                format!(
                    "{{\"t\":\"copy\",\"id\":{},\"from\":\"{}\",\"to\":\"{}\"}}",
                    id,
                    json_escape(&from),
                    json_escape(&to)
                ),
            )
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
    events: &Option<mpsc::Sender<RemoteEvent>>,
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
                    finalize_get(ws, transfers, status, events, id);
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
                if let Some(tx) = events {
                    let _ = tx.send(RemoteEvent::TransferDone { label: a.label().to_string(), ok });
                }
                finish_oneshot(status, ok);
            }
        }
        "error" => {
            let code = jstr(&json, "code").unwrap_or_default();
            let msg = jstr(&json, "msg").unwrap_or_default();
            // op responses come back with an `op` field; transfer errors don't
            if let Some(op) = jstr(&json, "op") {
                if let Some(tx) = events {
                    let _ = tx.send(RemoteEvent::OpResult {
                        op: op.to_string(),
                        ok: false,
                        msg: format!("[{}] {}", code, msg),
                    });
                }
                return;
            }
            if let Some(a) = transfers.remove(&id) {
                println!("  [xfer] FAILED '{}': [{}] {}", a.label(), code, msg);
                if let Some(tx) = events {
                    let _ = tx.send(RemoteEvent::TransferDone { label: a.label().to_string(), ok: false });
                }
                finish_oneshot(status, false);
            }
        }
        // ---- remote file-explorer ops (datalayer files.js) ----
        "dir" => {
            if let Some(tx) = events {
                let parsed: serde_json::Value =
                    serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                let mut entries = Vec::new();
                if let Some(arr) = parsed.get("entries").and_then(|v| v.as_array()) {
                    for e in arr {
                        entries.push(DirEntry {
                            name: e.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            is_dir: e.get("is_dir").and_then(|v| v.as_bool()).unwrap_or(false),
                            size: e.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                            mtime: e.get("mtime").and_then(|v| v.as_u64()),
                        });
                    }
                }
                let _ = tx.send(RemoteEvent::DirListing(DirListing {
                    path: parsed
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    entries,
                }));
            }
        }
        "mkdir_ok" | "delete_ok" | "rename_ok" | "copy_ok" => {
            let op = match t.as_str() {
                "mkdir_ok" => "mkdir",
                "delete_ok" => "delete",
                "copy_ok" => "copy",
                _ => "rename",
            };
            if let Some(tx) = events {
                let _ = tx.send(RemoteEvent::OpResult {
                    op: op.to_string(),
                    ok: true,
                    msg: String::new(),
                });
            }
        }
        _ => {}
    }
}

fn finalize_get(
    ws: &mut WebSocket<TcpStream>,
    transfers: &mut HashMap<u32, Active>,
    status: &Option<Arc<AtomicU8>>,
    events: &Option<mpsc::Sender<RemoteEvent>>,
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
    if let Some(cb) = events {
        let _ = cb.send(RemoteEvent::TransferDone { label: label.clone(), ok });
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
    events: &Option<mpsc::Sender<RemoteEvent>>,
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
        finalize_get(ws, transfers, status, events, id);
    }
}

/// Normalize a remote-cwd path relative to root. Clamps `..` at the root.
pub fn normalize_remote(cwd: &str, arg: &str) -> String {
    let joined = if arg.starts_with('/') || arg.contains(':') || arg == "." {
        arg.to_string()
    } else {
        format!("{}/{}", cwd.trim_end_matches('/'), arg)
    };
    let parts: Vec<&str> = joined.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let mut out: Vec<&str> = Vec::new();
    for seg in &parts {
        if *seg == ".." {
            out.pop();
        } else {
            out.push(seg);
        }
    }
    let result = out.join("/");
    if result.is_empty() { ".".to_string() } else { result }
}

/// Join a remote cwd with a relative path for transfer ops.
pub fn resolve_remote(cwd: &str, arg: &str) -> String {
    if arg.starts_with('/') || arg.contains(':') || arg == "." {
        arg.to_string()
    } else {
        let base = cwd.trim_end_matches('/');
        if base.is_empty() || base == "." {
            arg.to_string()
        } else {
            format!("{}/{}", base, arg)
        }
    }
}

/// Format a directory listing for display.
fn print_dir_listing(listing: &DirListing) {
    println!("  remote [{}]", listing.path);
    for e in &listing.entries {
        if e.is_dir {
            println!("    {:<20}  <DIR>", e.name);
        } else {
            println!("    {:<20}  {} bytes", e.name, e.size);
        }
    }
}

/// Drain and print all pending remote events (non-blocking).
fn drain_events(rx: &mpsc::Receiver<RemoteEvent>) {
    while let Ok(ev) = rx.try_recv() {
        match ev {
            RemoteEvent::DirListing(listing) => print_dir_listing(&listing),
            RemoteEvent::OpResult { op, ok, msg } => {
                if ok {
                    println!("  [remote] {} ok", op);
                } else if !msg.is_empty() {
                    println!("  [remote] {} FAILED: {}", op, msg);
                } else {
                    println!("  [remote] {} FAILED", op);
                }
            }
            RemoteEvent::TransferDone { label, ok } => {
                println!(
                    "  [xfer] '{}' {}",
                    label,
                    if ok { "done" } else { "FAILED" }
                );
            }
        }
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
    input_running: &Arc<AtomicBool>,
    input_handle: &mut Option<std::thread::JoinHandle<()>>,
    events_rx: &mpsc::Receiver<RemoteEvent>,
) -> Result<(), String> {
    let mut stdin_rx = if interactive { Some(spawn_stdin_reader()) } else { None };
    let mut stdin_done = false;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut last_ping_seen = Instant::now();
    let mut awaiting_result = false;
    let mut remote_cwd = String::from(".");
    loop {
        // drain remote events (dir listings, mkdir/delete/rename results)
        drain_events(events_rx);
        if let Some(rx) = &stdin_rx {
            while let Ok(m) = rx.try_recv() {
                match m {
                    StdinMsg::Eof => {
                        if creds::stdin_is_tty() {
                            if interactive {
                                println!();
                            }
                            if input_running.load(Ordering::Relaxed) {
                                input_running.store(false, Ordering::Relaxed);
                                if let Some(h) = input_handle.take() {
                                    let _ = h.join();
                                }
                            }
                            let _ = proto::write_frame(stream, BYE, b"");
                            return Ok(());
                        }
                        // Piped stdin closed (e.g. test harness): keep the
                        // session (and data link) alive instead of hanging up.
                        stdin_done = true;
                        break;
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
                        // ---- remote file-explorer verbs ----
                        if l == "pwd" {
                            println!("  {}", remote_cwd);
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if let Some(arg) = l.strip_prefix("cd ") {
                            remote_cwd = normalize_remote(&remote_cwd, arg.trim());
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if l == "cd" || l == "cd ~" || l == "cd ." {
                            remote_cwd = ".".to_string();
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if l == "ls" || l == "ll" || l.starts_with("ls ") || l.starts_with("ll ") {
                            let arg = l.split_whitespace().nth(1).unwrap_or(".");
                            let resolved = normalize_remote(&remote_cwd, arg);
                            let _ = xfer_tx.send(DlCommand::ListDir { path: resolved });
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if let Some(arg) = l.strip_prefix("mkdir ") {
                            let resolved = resolve_remote(&remote_cwd, arg.trim());
                            let _ = xfer_tx.send(DlCommand::CreateDir { path: resolved });
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if let Some(arg) = l.strip_prefix("rm ") {
                            let resolved = resolve_remote(&remote_cwd, arg.trim());
                            let _ = xfer_tx.send(DlCommand::Delete { path: resolved, recursive: false });
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if let Some(arg) = l.strip_prefix("rmdir ") {
                            let resolved = resolve_remote(&remote_cwd, arg.trim());
                            let _ = xfer_tx.send(DlCommand::Delete { path: resolved, recursive: true });
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if let Some(arg) = l.strip_prefix("del ") {
                            let resolved = resolve_remote(&remote_cwd, arg.trim());
                            let _ = xfer_tx.send(DlCommand::Delete { path: resolved, recursive: true });
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if let Some(arg) = l.strip_prefix("mv ") {
                            let parts: Vec<&str> = arg.splitn(2, |c| c == ' ' || c == '\t').collect();
                            if parts.len() == 2 {
                                let from = resolve_remote(&remote_cwd, parts[0].trim());
                                let to   = resolve_remote(&remote_cwd, parts[1].trim());
                                let _ = xfer_tx.send(DlCommand::Rename { old: from, new: to });
                            } else {
                                println!("  [i] usage: mv <source> <dest>");
                            }
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if let Some(arg) = l.strip_prefix("rename ") {
                            let parts: Vec<&str> = arg.splitn(2, |c| c == ' ' || c == '\t').collect();
                            if parts.len() == 2 {
                                let from = resolve_remote(&remote_cwd, parts[0].trim());
                                let to   = resolve_remote(&remote_cwd, parts[1].trim());
                                let _ = xfer_tx.send(DlCommand::Rename { old: from, new: to });
                            } else {
                                println!("  [i] usage: rename <source> <dest>");
                            }
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        // ---- transfer verbs (with remote-cwd resolution) ----
                        if l.starts_with("get ") {
                            let arg = l.strip_prefix("get ").unwrap_or("");
                            let mut it = arg.splitn(2, |c| c == ' ' || c == '\t');
                            let remote_arg = it.next().unwrap_or("").trim();
                            let local_arg = it.next().map(|s| s.trim()).unwrap_or("");
                            let local = if local_arg.is_empty() {
                                std::path::PathBuf::from(basename_of(remote_arg))
                            } else {
                                std::path::PathBuf::from(local_arg)
                            };
                            let resolved = resolve_remote(&remote_cwd, remote_arg);
                            let _ = xfer_tx.send(DlCommand::Get { remote: resolved, local });
                            println!("  [xfer] queued.");
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if l == "get" {
                            println!("  [i] usage: get <remote> [local]");
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if l.starts_with("put ") {
                            let arg = l.strip_prefix("put ").unwrap_or("");
                            let mut it = arg.splitn(2, |c| c == ' ' || c == '\t');
                            let local_str = it.next().unwrap_or("");
                            let remote_str = it.next().unwrap_or(local_str);
                            if std::path::Path::new(local_str).is_file() {
                                let resolved = resolve_remote(&remote_cwd, remote_str);
                                let _ = xfer_tx.send(DlCommand::Put {
                                    local: std::path::PathBuf::from(local_str),
                                    remote: resolved,
                                });
                                println!("  [xfer] queued.");
                            } else {
                                println!("  [xfer] no such file: {}", local_str);
                            }
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if l == "put" {
                            println!("  [i] usage: put <local> [remote]");
                            print!("pyielink> ");
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if l == "input start" || l == "input stop" {
                            handle_input_verb(l, xfer_tx, input_running, input_handle);
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
        if stdin_done {
            stdin_rx = None;
        }
        match proto::read_frame(stream) {
            Ok((PING, payload)) => {
                last_ping_seen = Instant::now();
                // In Shell REPL, heartbeat spam overwrites the pyielink> prompt — keep it quiet
                if !matches!(mode, RunMode::Shell) {
                    if let Ok(sent) = String::from_utf8_lossy(&payload).trim().parse::<u128>() {
                        println!("  [hb] rtt {}ms", now_ms().saturating_sub(sent));
                    } else {
                        println!("  [hb] alive");
                    }
                }
                if proto::write_frame(stream, PONG, &payload).is_err() {
                    return Err("lost connection to host".into());
                }
            }
            Ok((BYE, _)) => {
                println!("  [bye] host closed the session");
                if input_running.load(Ordering::Relaxed) {
                    input_running.store(false, Ordering::Relaxed);
                    if let Some(h) = input_handle.take() {
                        let _ = h.join();
                    }
                }
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
            Err(_) => {
                return Err("connection lost".into())
            }
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
        // EOF (piped/closed stdin) must fail fast, not spin forever.
        if std::io::stdin().read_line(&mut line).is_err() || line.is_empty() {
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
