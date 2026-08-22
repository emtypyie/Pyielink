use crate::creds;
use crate::proto::{self, *};
use crate::sessions;
use std::collections::HashMap;
use std::net::{IpAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DATA_PORT: &str = "4243"; // informational default; real port is ephemeral per session
const MAX_AUTH_ATTEMPTS: u32 = 3;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(5);
const PONG_GRACE: Duration = Duration::from_secs(15);

/* ---- per-IP failure throttling ---- */

const MAX_FAILS_PER_IP: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);

struct FailEntry {
    count: u32,
    locked_until: Option<Instant>,
}

static FAILS: std::sync::Mutex<Option<HashMap<IpAddr, FailEntry>>> = std::sync::Mutex::new(None);

fn with_fails<T>(f: impl FnOnce(&mut HashMap<IpAddr, FailEntry>) -> T) -> T {
    let mut g = FAILS.lock().unwrap();
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    f(g.as_mut().unwrap())
}

fn is_locked(ip: IpAddr) -> bool {
    let locked = with_fails(|m| {
        m.get(&ip).and_then(|e| e.locked_until).map_or(false, |t| Instant::now() < t)
    });
    locked
}

fn record_fail(ip: IpAddr) {
    with_fails(|m| {
        let e = m.entry(ip).or_insert(FailEntry { count: 0, locked_until: None });
        e.count += 1;
        if e.count >= MAX_FAILS_PER_IP {
            e.locked_until = Some(Instant::now() + LOCKOUT);
            e.count = 0;
            println!("  [lock] {} locked out {}s", ip, LOCKOUT.as_secs());
        }
    });
}

fn record_success(ip: IpAddr) {
    with_fails(|m| {
        m.remove(&ip);
    });
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

pub const DEFAULT_LICENSE: &str = "\
PYIELINK ETHICS & LICENSE AGREEMENT
===================================

pyielink grants remote access to THIS device. By accepting you agree:

 1. You are the owner of this device, or hold explicit written
    authorization from its owner to control it remotely.
 2. You will NOT use pyielink for unauthorized access, surveillance,
    or any action violating applicable law.
 3. The operator of this device may revoke access at any time by
    disabling remote access or removing your account.
 4. Connection tokens identify this client machine; keep them secret.

Type 'y' to accept and receive a connection token, or 'n' to abort.";

fn license_text() -> String {
    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.parent().unwrap_or(&exe).join("LICENSE.txt");
        if let Ok(body) = std::fs::read_to_string(candidate) {
            return body;
        }
    }
    DEFAULT_LICENSE.to_string()
}

fn send_fail(stream: &mut TcpStream, reason: &str) {
    let _ = proto::write_frame(stream, AUTH_FAIL, reason.as_bytes());
}

/// Refuse a connection that may have unread inbound data (e.g. we reject
/// before reading HELLO). Closing such a socket on Windows emits an RST
/// that can discard the queued refusal frame, so drain input first, send
/// the reason, then wait for the client to observe it before dropping.
fn refuse_and_close(mut stream: TcpStream, reason: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let mut sink = [0u8; 4096];
    loop {
        match std::io::Read::read(&mut stream, &mut sink) {
            Ok(n) if n > 0 => continue,
            _ => break,
        }
    }
    let _ = proto::write_frame(&mut stream, AUTH_FAIL, reason.as_bytes());
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    loop {
        match std::io::Read::read(&mut stream, &mut sink) {
            Ok(n) if n > 0 => continue,
            _ => break,
        }
    }
}

fn handle_conn(mut stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let peer_ip = stream.peer_addr().map(|a| a.ip()).unwrap_or_else(|_| "0.0.0.0".parse().unwrap());
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "unknown".into());
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));

    if is_locked(peer_ip) {
        println!("  [refused] {} — locked out", peer);
        refuse_and_close(stream, "too many failures, try later");
        return;
    }

    let state = creds::load_state();
    let (msg, payload) = match proto::read_frame(&mut stream) {
        Ok(x) => x,
        Err(_) => return,
    };
    if msg != HELLO {
        send_fail(&mut stream, "expected HELLO");
        return;
    }
    let hello = String::from_utf8_lossy(&payload).into_owned();
    let (user, _client_ver) = match hello.split_once('\n') {
        Some(x) => x,
        None => {
            send_fail(&mut stream, "malformed HELLO");
            return;
        }
    };

    if !state.enabled {
        println!("  [refused] {} — host disabled", peer);
        send_fail(&mut stream, "host disabled");
        return;
    }
    let record0 = match state.users.get(user) {
        Some(r) => r.clone(),
        None => {
            println!("  [refused] {} — unknown user '{}'", peer, user);
            send_fail(&mut stream, "unknown user");
            return;
        }
    };

    // Challenge-response auth: secrets never cross the wire.
    // CHALLENGE payload = "<pw_salt>\n<nonce>" (salt unused for token proofs;
    // client derives the identical stored secret locally before proving).
    let mut authenticated = false;
    let mut via_token = false;
    'auth: for attempt in 1..=MAX_AUTH_ATTEMPTS {
        let nonce = creds::new_nonce();
        let challenge = format!("{}\n{}", record0.pw_salt, nonce);
        if proto::write_frame(&mut stream, CHALLENGE, challenge.as_bytes()).is_err() {
            return;
        }
        let (msg, payload) = match proto::read_frame(&mut stream) {
            Ok(x) => x,
            Err(_) => return,
        };
        if msg != PROOF {
            send_fail(&mut stream, "expected PROOF");
            return;
        }
        let line = String::from_utf8_lossy(&payload).into_owned();
        let (mode, proof) = match line.split_once(':') {
            Some(x) => x,
            None => {
                send_fail(&mut stream, "malformed PROOF");
                return;
            }
        };
        let ok = match mode {
            "t" => !record0.token_hash.is_empty()
                && creds::verify_proof(&record0.token_hash, &nonce, proof),
            "p" => creds::verify_proof(&record0.pw_hash, &nonce, proof),
            _ => false,
        };
        if ok {
            authenticated = true;
            via_token = mode == "t";
            break 'auth;
        }
        record_fail(peer_ip);
        println!(
            "  [auth] {} — bad {} proof (attempt {}/{})",
            peer,
            if mode == "t" { "token" } else { "password" },
            attempt,
            MAX_AUTH_ATTEMPTS
        );
        if is_locked(peer_ip) {
            send_fail(&mut stream, "too many failures, try later");
            return;
        }
    }
    if !authenticated {
        println!("  [denied] {} — '{}' exhausted auth attempts", peer, user);
        send_fail(&mut stream, "authentication failed");
        return;
    }
    record_success(peer_ip);

    // License gate (skipped for returning token sessions)
    if !record0.licensed {
        if proto::write_frame(&mut stream, LICENSE_TEXT, license_text().as_bytes()).is_err() {
            return;
        }
        match proto::read_frame(&mut stream) {
            Ok((LICENSE_ACCEPT, _)) => {}
            Ok((LICENSE_REJECT, _)) => {
                println!("  [license] '{}' rejected the agreement — closing", user);
                return;
            }
            _ => return,
        }
        let mut st = creds::load_state();
        if let Some(rec) = st.users.get_mut(user) {
            rec.licensed = true;
        }
        if creds::save_state(&st).is_err() {
            send_fail(&mut stream, "host state error");
            return;
        }
    }

    // Password path rotates the connection token; token path keeps it
    if !via_token {
        let new_token = crate::token::generate();
        let hash = crate::token::hash_token(&new_token);
        let mut st = creds::load_state();
        if let Some(rec) = st.users.get_mut(user) {
            rec.token_hash = hash;
        }
        if creds::save_state(&st).is_err()
            || proto::write_frame(&mut stream, TOKEN_ISSUED, new_token.as_bytes()).is_err()
        {
            return;
        }
    }

    // Promotion ticket: data port + one-time session key for the data layer.
    // The Node data layer is spawned per promoted session; refusal to promote
    // (no node runtime / no scripts / no free port) fails the whole login so
    // the client never ends up half-promoted.
    let session_key = sessions::open_session(user, peer_ip);
    let role = if record0.is_admin() { "admin" } else { "user" };
    let (mut datalayer, dl_port) = match spawn_datalayer(&session_key, user, role) {
        Ok(x) => x,
        Err(reason) => {
            println!("  [denied] {} — promotion failed: {}", peer, reason);
            let _ = proto::write_frame(&mut stream, AUTH_FAIL, reason.as_bytes());
            sessions::close(&session_key);
            return;
        }
    };
    let ticket = format!("{}\n{}", dl_port, session_key);
    if proto::write_frame(&mut stream, AUTH_OK, ticket.as_bytes()).is_ok() {
        println!(
            "  [ok] {} promoted as '{}' ({}, data port {}, terminal attached)",
            peer,
            user,
            if record0.is_admin() { "admin" } else { "standard" },
            dl_port
        );
    }

    session_loop(&mut stream, record0.is_admin(), user, &session_key, &mut Some(datalayer));
}

enum WorkerMsg {
    Out(Vec<u8>),
    Done(i32),
}

/// Single-threaded post-auth loop: services heartbeats AND the remote
/// terminal channel on one socket. Commands run on worker threads that
/// stream output back through a channel, so PING/PONG never stalls while
/// a long command executes. One command at a time per session (MVP).
/// `datalayer` is the Node child serving this session's data port; it is
/// killed on every exit path (BYE, heartbeat loss, dead socket, TTL).
fn session_loop(
    stream: &mut TcpStream,
    is_admin: bool,
    user: &str,
    session_key: &str,
    datalayer: &mut Option<std::process::Child>,
) {
    const POLL: Duration = Duration::from_millis(200);
    let mut last_pong = Instant::now();
    let mut last_ping_sent = Instant::now() - PING_INTERVAL; // ping immediately
    let mut worker: Option<std::sync::mpsc::Receiver<WorkerMsg>> = None;
    let _ = stream.set_read_timeout(Some(POLL));
    loop {
        // forward whatever the command worker produced
        if let Some(rx) = &worker {
            loop {
                match rx.try_recv() {
                    Ok(WorkerMsg::Out(chunk)) => {
                        if proto::write_frame(stream, EXEC_OUT, &chunk).is_err() {
                            break;
                        }
                    }
                    Ok(WorkerMsg::Done(code)) => {
                        let _ = proto::write_frame(stream, EXEC_END, code.to_string().as_bytes());
                        worker = None;
                        break;
                    }
                    Err(_) => break,
                }
            }
            if worker.is_none() {
                continue; // re-poll promptly after completion
            }
        }
        // heartbeat when due
        if last_ping_sent.elapsed() >= PING_INTERVAL {
            if proto::write_frame(stream, PING, now_ms().to_string().as_bytes()).is_err() {
                break;
            }
            last_ping_sent = Instant::now();
        }
        match proto::read_frame(stream) {
            Ok((PONG, _)) => {
                last_pong = Instant::now();
                if !sessions::touch(session_key) {
                    break;
                }
            }
            Ok((PING, payload)) => {
                // symmetric support: mirror any client-side ping
                let _ = proto::write_frame(stream, PONG, &payload);
                last_pong = Instant::now();
            }
            Ok((BYE, _)) => {
                println!("  [bye] {} closed the session cleanly", user);
                break;
            }
            Ok((EXEC_REQ, payload)) => {
                if payload.is_empty() {
                    continue;
                }
                let elevated = payload[0] == b'1';
                let cmd = String::from_utf8_lossy(&payload[1..]).trim().to_string();
                if cmd.is_empty() {
                    continue;
                }
                if elevated && !is_admin {
                    println!("  [exec] {} denied elevated command (standard role)", user);
                    let _ = proto::write_frame(
                        stream,
                        EXEC_DENY,
                        b"'sudo' requires an admin account on this host",
                    );
                    continue;
                }
                if worker.is_some() {
                    let _ = proto::write_frame(
                        stream,
                        EXEC_DENY,
                        b"a command is still running on this session",
                    );
                    continue;
                }
                println!(
                    "  [exec] {} runs {}{:?}",
                    user,
                    if elevated { "elevated " } else { "" },
                    cmd
                );
                worker = Some(spawn_command(&cmd));
            }
            Ok(_) => {} // tolerate unknown/legacy frames
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if last_pong.elapsed() >= PONG_GRACE {
                    println!("  [lost] {} missed heartbeats — closing session", user);
                    break;
                }
            }
            Err(_) => break, // connection dead
        }
    }
    sessions::close(session_key);
    if let Some(child) = datalayer {
        let _ = child.kill();
        let _ = child.wait();
        println!("  [dl] data layer for '{}' stopped", user);
    }
}

/* ---- Node data-layer lifecycle (Phase 2) ---- */

fn node_on_path() -> bool {
    use std::process::{Command, Stdio};
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Locate datalayer/src/server.js: PYIELINK_DATALAYER override first, then
/// next to the executable, then the working directory.
fn datalayer_script() -> Option<std::path::PathBuf> {
    const REL: [&str; 3] = ["datalayer", "src", "server.js"];
    let mut bases: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("PYIELINK_DATALAYER") {
        bases.push(std::path::PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            bases.push(dir.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        bases.push(cwd);
    }
    bases.into_iter().map(|b| b.iter().collect::<std::path::PathBuf>()).find_map(|base| {
        let p = REL.iter().fold(base, |acc, part| acc.join(part));
        if p.is_file() { Some(p) } else { None }
    })
}

/// Grab an ephemeral loopback port for this session's data layer so
/// concurrent/overlapping sessions never collide on a fixed port.
fn pick_free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

/// Write the single-use handoff file ("<key>\n<user>\n<role>") and spawn the
/// Node server with PYIELINK_SESSION pointing at it; the child deletes the
/// file on startup so the key never lingers on disk.
fn spawn_datalayer(
    session_key: &str,
    user: &str,
    role: &str,
) -> Result<(std::process::Child, u16), String> {
    use std::process::{Command, Stdio};
    if !node_on_path() {
        return Err("data layer unavailable: node runtime not found on PATH".into());
    }
    let script = datalayer_script()
        .ok_or_else(|| "data layer unavailable: datalayer/src/server.js not found".to_string())?;
    let port = pick_free_port().ok_or_else(|| "no free local port for data layer".to_string())?;
    let handoff = std::env::temp_dir().join(format!("pyielink-session-{}.env", crate::token::generate()));
    std::fs::write(&handoff, format!("{}\n{}\n{}\n", session_key, user, role))
        .map_err(|e| format!("cannot write session handoff: {}", e))?;
    let attempt = Command::new("node")
        .arg(&script)
        .arg("--port")
        .arg(port.to_string())
        .env("PYIELINK_SESSION", &handoff)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    match attempt {
        Ok(mut child) => {
            if let Some(out) = child.stdout.take() {
                std::thread::spawn(move || drain_to_console(out, "dl"));
            }
            if let Some(err) = child.stderr.take() {
                std::thread::spawn(move || drain_to_console(err, "dl!"));
            }
            Ok((child, port))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&handoff);
            Err(format!("cannot start data layer: {}", e))
        }
    }
}

/// Forward Node child telemetry to the host console (and therefore into the
/// host's redirected output in headless runs).
fn drain_to_console<R: std::io::Read + Send + 'static>(pipe: R, tag: &'static str) {
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(pipe);
    for line in reader.lines().map_while(Result::ok) {
        println!("  [{}] {}", tag, line);
    }
}

const EXEC_OUT_CAP: usize = 512 * 1024;

#[cfg(windows)]
fn shell_for_exec() -> (&'static str, &'static str) {
    ("cmd", "/C")
}

#[cfg(not(windows))]
fn shell_for_exec() -> (&'static str, &'static str) {
    ("sh", "-c")
}

fn spawn_command(cmd: &str) -> std::sync::mpsc::Receiver<WorkerMsg> {
    let (tx, rx) = std::sync::mpsc::channel();
    let cmd = cmd.to_string();
    std::thread::spawn(move || {
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let (prog, flag) = shell_for_exec();
        let attempt =
            Command::new(prog).args([flag]).arg(&cmd).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let mut child = match attempt {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(WorkerMsg::Out(
                    format!("pyielink: cannot start command: {}\r\n", e).into_bytes(),
                ));
                let _ = tx.send(WorkerMsg::Done(127));
                return;
            }
        };
        let remaining = std::sync::Arc::new(AtomicUsize::new(EXEC_OUT_CAP));
        let truncated = std::sync::Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        let mut pipes: Vec<Box<dyn std::io::Read + Send>> = Vec::new();
        if let Some(o) = child.stdout.take() {
            pipes.push(Box::new(o));
        }
        if let Some(e) = child.stderr.take() {
            pipes.push(Box::new(e));
        }
        for pipe in pipes {
            let tx = tx.clone();
            let remaining = std::sync::Arc::clone(&remaining);
            let truncated = std::sync::Arc::clone(&truncated);
            handles.push(std::thread::spawn(move || {
                use std::io::Read;
                let mut r = pipe;
                let mut buf = [0u8; 8192];
                loop {
                    let left = remaining.load(Ordering::Relaxed);
                    if left == 0 {
                        if !truncated.swap(true, Ordering::Relaxed) {
                            let _ =
                                tx.send(WorkerMsg::Out(b"\r\n...output truncated...\r\n".to_vec()));
                        }
                        return;
                    }
                    let want = buf.len().min(left);
                    match r.read(&mut buf[..want]) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            remaining.fetch_sub(n, Ordering::Relaxed);
                            if tx.send(WorkerMsg::Out(buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = tx.send(WorkerMsg::Done(code));
    });
    rx
}

pub fn listen(port: u16) -> Result<(), String> {
    let addr = format!("0.0.0.0:{}", port);
    let listener =
        TcpListener::bind(&addr).map_err(|e| format!("cannot bind {}: {}", addr, e))?;
    println!("  [ok] pyielink host listening on {} (data port assigned per session)", addr);
    if !node_on_path() {
        println!("  [warn] node runtime not found on PATH — promoted sessions will be refused until it is installed");
    } else if datalayer_script().is_none() {
        println!("  [warn] datalayer/src/server.js not found — set PYIELINK_DATALAYER or ship the datalayer folder with the binary");
    }
    let ips = local_ips();
    if ips.is_empty() {
        println!("       this device: (no network interfaces detected)");
    } else {
        for ip in &ips {
            println!("       this device reachable at: {}", ip);
        }
        println!(
            "       clients connect with: pyielink <user>@{}",
            ips.iter().find(|i| !i.is_loopback()).unwrap_or(&ips[0])
        );
    }
    println!("       waiting for connections... Ctrl+C to stop.");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                std::thread::spawn(move || handle_conn(stream));
            }
            Err(e) => println!("  [warn] accept failed: {}", e),
        }
    }
    Ok(())
}

/// Best-effort address discovery for the /enable banner: primary outbound
/// address first (UDP-connect trick sends no packets), then every address
/// the hostname resolves to.
fn local_ips() -> Vec<IpAddr> {
    use std::net::{ToSocketAddrs, UdpSocket};
    let mut out: Vec<IpAddr> = Vec::new();
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(a) = s.local_addr() {
                out.push(a.ip());
            }
        }
    }
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    if !host.is_empty() {
        if let Ok(addrs) = (host.as_str(), 0u16).to_socket_addrs() {
            for a in addrs {
                if !out.contains(&a.ip()) {
                    out.push(a.ip());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_locks_after_threshold() {
        let ip: IpAddr = "203.0.113.77".parse().unwrap();
        with_fails(|m| {
            m.remove(&ip);
        });
        for _ in 0..MAX_FAILS_PER_IP {
            record_fail(ip);
        }
        assert!(is_locked(ip));
        record_success(ip);
        assert!(!is_locked(ip));
    }

    #[test]
    fn now_ms_sane() {
        assert!(now_ms() > 1_700_000_000_000);
    }
}
