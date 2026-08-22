use crate::creds;
use crate::proto::{self, *};
use crate::token;
use std::io::Write;
use std::net::{TcpStream, UdpSocket};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
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
    let (user, ip) = parse_target(target)?;
    let addr = format!("{}:{}", ip, BOOTSTRAP_PORT);
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
                        None => ("p", password_proof(&salt, &nonce)),
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
                let dl_handle = {
                    let (ip, port, key, state, stop) =
                        (ip.clone(), data_port.clone(), session_key.clone(), Arc::clone(&dl_state), Arc::clone(&dl_stop));
                    std::thread::spawn(move || data_link_loop(&ip, &port, &key, state, stop))
                };
                let interactive = std::env::var("PYIELINK_SHELL").ok().as_deref() == Some("1")
                    || creds::stdin_is_tty();
                if interactive {
                    println!("  [i] remote terminal ready — type a command ('sudo <cmd>' for elevated, 'exit' to quit)");
                    print!("pyielink> ");
                    let _ = std::io::stdout().flush();
                }
                let outcome = post_auth_loop(&mut stream, interactive, &dl_state);
                dl_stop.store(1, Ordering::Relaxed);
                let _ = dl_handle.join();
                outcome?;
                return Ok(());
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

/// Native data-plane client: ws handshake with the session key, then a
/// control-channel heartbeat service (answer PINGs, emit own PINGs, RTT log)
/// under a staleness watchdog. Exits when `stop` flips to 1 or the link dies.
fn data_link_loop(
    ip: &str,
    port: &str,
    key: &str,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicU8>,
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
    loop {
        if stop.load(Ordering::Relaxed) == 1 {
            dl_shutdown(&mut ws);
            return;
        }
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
                _ => {} // other channels arrive in later phases; ignore unknown
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
                eprintln!("  [dl-link] read failed: {}", e);
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

/// Root channel: answers host pings, streams remote-command output, and in
/// interactive mode feeds typed lines to the host's terminal channel.
fn post_auth_loop(
    stream: &mut TcpStream,
    interactive: bool,
    dl_state: &AtomicU8,
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
