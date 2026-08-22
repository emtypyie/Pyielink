use crate::creds;
use crate::proto::{self, *};
use crate::sessions;
use std::collections::HashMap;
use std::net::{IpAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DATA_PORT: &str = "4243";
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

    // Promotion ticket: data port + one-time session key for the data layer
    let session_key = sessions::open_session(user, peer_ip);
    let ticket = format!("{}\n{}", DATA_PORT, session_key);
    if proto::write_frame(&mut stream, AUTH_OK, ticket.as_bytes()).is_ok() {
        println!("  [ok] {} promoted as '{}' (data port {}, session open)", peer, user, DATA_PORT);
    }

    heartbeat_loop(&mut stream, &session_key, user);
}

/// Persistent root channel: host pings every PING_INTERVAL, tears down when
/// pongs stop arriving. The bootstrap socket stays alive for Phase 2 channels.
fn heartbeat_loop(stream: &mut TcpStream, session_key: &str, user: &str) {
    let mut last_pong = Instant::now();
    let mut last_ping_sent = Instant::now() - PING_INTERVAL; // ping immediately
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    loop {
        if last_ping_sent.elapsed() >= PING_INTERVAL {
            if proto::write_frame(stream, PING, now_ms().to_string().as_bytes()).is_err() {
                break;
            }
            last_ping_sent = Instant::now();
        }
        match proto::read_frame(stream) {
            Ok((PONG, payload)) => {
                last_pong = Instant::now();
                if !sessions::touch(session_key) {
                    break; // expired elsewhere
                }
                if let Ok(sent) = String::from_utf8_lossy(&payload).trim().parse::<u128>() {
                    let rtt = now_ms().saturating_sub(sent);
                    println!("  [hb] {} rtt {}ms", user, rtt);
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
            Ok((msg, _)) => {
                println!("  [warn] unexpected frame 0x{:02X} on control channel — closing", msg);
                break;
            }
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
}

pub fn listen(port: u16) -> Result<(), String> {
    let addr = format!("0.0.0.0:{}", port);
    let listener =
        TcpListener::bind(&addr).map_err(|e| format!("cannot bind {}: {}", addr, e))?;
    println!("  [ok] pyielink host listening on {} (data port {})", addr, DATA_PORT);
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
