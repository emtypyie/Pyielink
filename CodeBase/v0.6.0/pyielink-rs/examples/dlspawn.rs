use std::io::Write;
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;
use tungstenite::Message;

fn main() {
    let port: u16 = 61337;
    let script =
        "C:\\Users\\workf\\AppData\\Roaming\\npm\\node_modules\\pyielink\\datalayer\\src\\server.js";
    let handoff =
        std::env::temp_dir().join("dlspawn-env.env");
    std::fs::write(&handoff, "abc123\ntestuser\nstandard\n").unwrap();

    println!("[harness] spawning node (host-style) on port {}", port);
    let child = Command::new("node")
        .arg(script)
        .arg("--port")
        .arg(port.to_string())
        .env("PYIELINK_SESSION", &handoff)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            println!("[harness] SPAWN FAILED: {}", e);
            return;
        }
    };
    println!("[harness] spawned pid={}", child.id());
    println!("[harness] NODE_READY port={}", port);
    std::thread::sleep(Duration::from_secs(30));
    match child.try_wait() {
        Ok(Some(s)) => println!("[harness] node already exited: {:?}", s),
        Ok(None) => {
            println!("[harness] node still alive; killing");
            let _ = child.kill();
        }
        Err(e) => println!("[harness] try_wait err: {}", e),
    }
    let _ = std::fs::remove_file(&handoff);
    let _ = writeln!(std::io::stdout(), "[harness] done");
}
