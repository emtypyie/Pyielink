use crate::creds;
use crate::proto::{self, *};
use std::net::{TcpListener, TcpStream};

pub const DATA_PORT: &str = "4243";
const MAX_AUTH_ATTEMPTS: u32 = 3;

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

fn handle_conn(mut stream: TcpStream) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".into());

    // Fresh state per connection so concurrent sessions see updates
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

    // Authentication: up to N attempts mixing token / password replies
    if proto::write_frame(&mut stream, PASSWD_REQ, b"").is_err() {
        return;
    }
    let mut authenticated = false;
    let mut via_token = false;
    for attempt in 1..=MAX_AUTH_ATTEMPTS {
        let (msg, payload) = match proto::read_frame(&mut stream) {
            Ok(x) => x,
            Err(_) => return,
        };
        match msg {
            AUTH_TOKEN => {
                let token = String::from_utf8_lossy(&payload);
                if !record0.token_hash.is_empty()
                    && crate::token::hash_token(token.trim()) == record0.token_hash
                {
                    authenticated = true;
                    via_token = true;
                    break;
                }
                println!("  [auth] {} — bad token (attempt {}/{})", peer, attempt, MAX_AUTH_ATTEMPTS);
            }
            PASSWD_AUTH => {
                let password = String::from_utf8_lossy(&payload);
                if creds::verify_password(&record0, &password) {
                    authenticated = true;
                    break;
                }
                println!("  [auth] {} — bad password (attempt {}/{})", peer, attempt, MAX_AUTH_ATTEMPTS);
            }
            _ => {
                send_fail(&mut stream, "unexpected message during auth");
                return;
            }
        }
        if proto::write_frame(&mut stream, PASSWD_REQ, b"").is_err() {
            return;
        }
    }
    if !authenticated {
        println!("  [denied] {} — '{}' exhausted auth attempts", peer, user);
        send_fail(&mut stream, "authentication failed");
        return;
    }

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

    // Password path issues a fresh token; token path keeps the existing one
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

    if proto::write_frame(&mut stream, AUTH_OK, DATA_PORT.as_bytes()).is_ok() {
        println!("  [ok] {} promoted as '{}' (data port {})", peer, user, DATA_PORT);
    }
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
