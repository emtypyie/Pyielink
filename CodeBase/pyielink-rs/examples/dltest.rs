use std::net::TcpStream;
use std::time::Duration;
use tungstenite::Message;

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:61337/".to_string());
    let key = std::env::args().nth(2).unwrap_or_else(|| "abc123".to_string());
    let body = url.trim_start_matches("ws://");
    let (hostport, _) = body.split_once('/').unwrap_or((body, ""));
    println!("connecting to {} for url {}", hostport, url);
    let mut tcp = match TcpStream::connect(hostport) {
        Ok(t) => t,
        Err(e) => {
            println!("TCP CONNECT FAILED: {}", e);
            return;
        }
    };
    tcp.set_read_timeout(Some(Duration::from_secs(3))).ok();
    match tungstenite::client(&url, tcp) {
        Ok((mut ws, resp)) => {
            println!("HANDSHAKE OK status={:?}", resp.status());
            let msg = format!("{{\"k\":\"{}\"}}", key);
            if ws.write(Message::Text(msg)).is_err() {
                println!("SEND KEY FAILED");
                return;
            }
            println!("KEY SENT, waiting for ack...");
            match ws.read() {
                Ok(Message::Text(t)) => println!("ACK: {}", t),
                Ok(other) => println!("OTHER: {:?}", other),
                Err(e) => println!("READ ERR: {}", e),
            }
        }
        Err(e) => println!("HANDSHAKE ERR: {}", e),
    }
}
