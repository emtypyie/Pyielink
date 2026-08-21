use crate::creds;
use crate::proto::{self, *};
use crate::token;
use std::io::Write;
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

const BOOTSTRAP_PORT: u16 = 4242;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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

pub fn run_connect(target: &str) -> Result<(), String> {
    let (user, ip) = parse_target(target)?;
    let addr = format!("{}:{}", ip, BOOTSTRAP_PORT);
    println!("  [..] connecting to {} as '{}' ...", addr, user);

    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| format!("cannot reach {}: {}", addr, e))?;
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|e| e.to_string())?;

    let hello = format!("{}\n{}\n", user, env!("CARGO_PKG_VERSION"));
    proto::write_frame(&mut stream, HELLO, hello.as_bytes())
        .map_err(|e| format!("handshake send failed: {}", e))?;

    // Auth loop: prefer the stored token once, then fall back to passwords
    let mut sent_token = false;
    let mut attempts = 0u32;
    loop {
        match expect_frame(&mut stream)? {
            (PASSWD_REQ, _) => {
                let stored = if sent_token { None } else { token::load_client_token(&user, &ip) };
                match stored.filter(|t| !t.is_empty()) {
                    Some(tok) => {
                        sent_token = true;
                        println!("  [..] presenting stored connection token...");
                        proto::write_frame(&mut stream, AUTH_TOKEN, tok.as_bytes())
                            .map_err(|e| format!("send failed: {}", e))?;
                    }
                    None => {
                        if attempts >= 3 {
                            return Err("too many failed password attempts".into());
                        }
                        let pw = prompt_password();
                        attempts += 1;
                        proto::write_frame(&mut stream, PASSWD_AUTH, pw.as_bytes())
                            .map_err(|e| format!("send failed: {}", e))?;
                    }
                }
            }
            (LICENSE_TEXT, payload) => {
                println!("\n{}", String::from_utf8_lossy(&payload));
                if !confirm_license() {
                    proto::write_frame(&mut stream, LICENSE_REJECT, b"n")
                        .map_err(|e| e.to_string())?;
                    return Err("license rejected — session aborted".into());
                }
                proto::write_frame(&mut stream, LICENSE_ACCEPT, b"y")
                    .map_err(|e| e.to_string())?;
            }
            (TOKEN_ISSUED, payload) => {
                let tok = String::from_utf8_lossy(&payload).trim().to_string();
                let path = token::save_client_token(&user, &ip, &tok)
                    .map_err(|e| format!("could not store token: {}", e))?;
                println!("  [ok] connection token stored at {}", path.display());
            }
            (AUTH_OK, payload) => {
                let data_port = String::from_utf8_lossy(&payload).trim().to_string();
                println!(
                    "  [ok] session promoted. data layer ready on {}:{}. (channels arrive in phase 2)",
                    ip, data_port
                );
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
