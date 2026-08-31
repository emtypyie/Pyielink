use std::env;
use std::process::{self, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::io::{BufRead, BufReader};

use pyielink::creds::{add_to_whitelist, remove_from_whitelist, add_user, cmd_enable_with_flags};

/// Read a port from the given env var, defaulting to `fallback` on missing/invalid values.
fn env_port(name: &str, fallback: u16) -> u16 {
    std::env::var(name).ok().and_then(|p| p.parse().ok()).unwrap_or(fallback)
}

fn print_usage() {
    eprintln!("usage: pyielink <command> [args...]");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  user@ip                   Connect to host (GUI mode)");
    eprintln!("  user@ip --repl            Connect to host (REPL terminal mode)");
    eprintln!("  --repl user@ip            Connect to host (REPL terminal mode)");
    eprintln!("  enable                  Enable host for connections");
    eprintln!("  enable --all             Enable host for connections from any IP");
    eprintln!("  enable --whitelist IP    Allow connections from specific IP");
    eprintln!("  adduser -m <name>        Create a host user account (prompts for password)");
    eprintln!("  adduser -m <name> -r <role>  Create account with role user|admin");
    eprintln!("  whitelist add IP         Add IP to connection whitelist");
    eprintln!("  whitelist remove IP      Remove IP from connection whitelist");
    eprintln!("  tunnel start             Start tunnel (cloudflared, requires binary)");
    eprintln!("  host                     Start the host listener (accept connections on port 4242)");
    eprintln!();
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    // Handle --version / -v / --help / -h up front.
    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("pyielink {}", env!("CARGO_PKG_VERSION"));
        process::exit(0);
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        process::exit(0);
    }

    if args.is_empty() {
        print_usage();
        process::exit(1);
    }

    // --repl may appear as `pyielink --repl user@ip` or `pyielink user@ip --repl` (user request)
    if args.iter().any(|a| a == "--repl") {
        let target = args.iter().find(|a| *a != "--repl" && a.contains('@'));
        match target {
            Some(t) => {
                if let Err(e) = pyielink::client::run_connect(t, true) {
                    eprintln!("  [error] connection failed: {}", e);
                    process::exit(1);
                }
                return;
            }
            None => {
                eprintln!("  [error] usage: pyielink user@ip --repl  or  pyielink --repl user@ip");
                process::exit(1);
            }
        }
    }

    let command = &args[0];

    match command.as_str() {
        "enable" => {
            let allow_all = args.contains(&"--all".to_string());
            let mut whitelist: Vec<String> = Vec::new();

            // Parse --whitelist flags (can appear multiple times)
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--whitelist" && i + 1 < args.len() {
                    whitelist.push(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }

            match cmd_enable_with_flags(allow_all, whitelist) {
                Ok(()) => println!("  [ok] host enabled"),
                Err(e) => eprintln!("  [error] {}", e),
            }
        }

        "adduser" => {
            let mut name: Option<String> = None;
            let mut role = String::from("user");

            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "-m" | "--name" => {
                        if i + 1 < args.len() {
                            name = Some(args[i + 1].clone());
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    "-r" | "--role" => {
                        if i + 1 < args.len() {
                            role = args[i + 1].clone();
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    other => {
                        if name.is_none() {
                            name = Some(other.to_string());
                        }
                        i += 1;
                    }
                }
            }

            match name {
                Some(n) => match add_user(&n, &role) {
                    Ok(()) => {}
                    Err(e) => eprintln!("  [error] {}", e),
                },
                None => eprintln!("  [error] usage: pyielink adduser -m <name> [-r user|admin]"),
            }
        }

        "whitelist" => {
            if args.len() < 3 {
                eprintln!("  [error] usage: pyielink whitelist add|remove <IP>");
                process::exit(1);
            }

            let action = &args[1];
            let ip = &args[2];

            match action.as_str() {
                "add" => {
                    match add_to_whitelist(ip.clone()) {
                        Ok(()) => println!("  [ok] IP '{}' added to whitelist", ip),
                        Err(e) => eprintln!("  [error] {}", e),
                    }
                }
                "remove" => {
                    match remove_from_whitelist(ip) {
                        Ok(()) => println!("  [ok] IP '{}' removed from whitelist", ip),
                        Err(e) => eprintln!("  [error] {}", e),
                    }
                }
                _ => {
                    eprintln!("  [error] unknown whitelist action: {}", action);
                    print_usage();
                    process::exit(1);
                }
            }
        }

        "tunnel" => {
            if args.len() < 2 {
                eprintln!("  [error] usage: pyielink tunnel start [cloudflared|nginx]");
                process::exit(1);
            }

            match args[1].as_str() {
                "start" => {
                    let tunnel_type = args.get(2).map(|s| s.as_str()).unwrap_or("cloudflared");
                    match tunnel_type {
                        "cloudflared" => run_tunnel_cloudflared(),
                        "nginx" => run_tunnel_nginx(),
                        other => {
                            eprintln!("  [error] unknown tunnel type '{}' (use cloudflared or nginx)", other);
                            process::exit(1);
                        }
                    }
                }
                other => {
                    eprintln!("  [error] unknown tunnel subcommand: {}", other);
                    print_usage();
                    process::exit(1);
                }
            }
        }

        "host" => {
            // Start accepting incoming connections on the bootstrap port.
            const PORT: u16 = 4242;
            if let Err(e) = pyielink::host::listen(PORT) {
                eprintln!("  [error] {}", e);
                process::exit(1);
            }
        }

        _ => {
            // Default: connect to host (GUI mode)
            if let Err(e) = pyielink::client::run_connect(command, false) {
                eprintln!("  [error] connection failed: {}", e);
                process::exit(1);
            }
        }
    }
}

/// Spawn a cloudflared quick TCP tunnel to `localhost:<local_port>` and watch
/// its output for the assigned `tcp://host:port` address.
fn spawn_cloudflared(label: &str, local_port: u16, found: &Arc<Mutex<Option<String>>>) -> Option<std::process::Child> {
    let mut child = Command::new("cloudflared")
        .args(["tunnel", "--url", &format!("tcp://localhost:{}", local_port)])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let label = label.to_string();
    let found = Arc::clone(found);

    // Parse the assigned tcp://host:port out of cloudflared's stdout.
    if let Some(out) = child.stdout.take() {
        let found = Arc::clone(&found);
        let label = label.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().flatten() {
                if let Some(idx) = line.find("tcp://") {
                    let rest = &line[idx + "tcp://".len()..];
                    let end = rest
                        .find(|c| c == ' ' || c == '"' || c == '|' || c == '}' || c == '\n')
                        .unwrap_or(rest.len());
                    let addr = rest[..end].to_string();
                    if !addr.is_empty() {
                        if let Ok(mut g) = found.lock() {
                            *g = Some(addr);
                        }
                    }
                }
                println!("  [cloudflared/{}] {}", label, line);
            }
        });
    }

    // Mirror stderr so cloudflared stays unblocked and errors are visible.
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().flatten() {
                println!("  [cloudflared/{}] {}", label, line);
            }
        });
    }

    Some(child)
}

fn run_tunnel_cloudflared() {
    let bootstrap_port = env_port("PYIELINK_PORT", 4242);
    let data_port = env_port("PYIELINK_DATA_PORT", 4243);

    println!(
        "  [tunnel] starting cloudflared quick tunnels (bootstrap :{}, data :{})",
        bootstrap_port, data_port
    );
    println!("  [tunnel] requires 'cloudflared' on PATH — https://github.com/cloudflare/cloudflared/releases");

    let bootstrap_found = Arc::new(Mutex::new(None::<String>));
    let data_found = Arc::new(Mutex::new(None::<String>));

    let mut bootstrap_child = match spawn_cloudflared("bootstrap", bootstrap_port, &bootstrap_found) {
        Some(c) => c,
        None => { eprintln!("  [error] could not launch cloudflared — install it and ensure it is on PATH"); process::exit(1); }
    };
    // Kept alive for the process lifetime so the data tunnel isn't dropped.
    let _data_child = match spawn_cloudflared("data", data_port, &data_found) {
        Some(c) => c,
        None => {
            let _ = bootstrap_child.kill();
            eprintln!("  [error] could not launch cloudflared — install it and ensure it is on PATH");
            process::exit(1);
        }
    };

    // Give cloudflared a moment to register the tunnels and print addresses.
    std::thread::sleep(std::time::Duration::from_secs(4));

    let b_addr = bootstrap_found.lock().unwrap().clone();
    let d_addr = data_found.lock().unwrap().clone();

    if let (Some(b), Some(d)) = (&b_addr, &d_addr) {
        let path = std::env::temp_dir().join("pyielink-tunnel.txt");
        let content = format!(
            "bootstrap_local={}\nbootstrap={}\ndata_local={}\ndata={}\n",
            bootstrap_port, b, data_port, d
        );
        let _ = std::fs::write(&path, content);
        println!();
        println!("  [ok] tunnel live:");
        println!("    bootstrap : {}", b);
        println!("    data      : {}", d);
        println!();
        println!("  HOST: keep 'pyielink host' running (it auto-reads this tunnel).");
        println!("  CLIENT connects with:");
        println!("    pyielink emty@{}", b);
        println!();
        println!("  (Ctrl-C stops the tunnels)");
    } else {
        println!();
        println!("  [warn] cloudflared started but public addresses not parsed yet;");
        println!("  [warn] check the cloudflared logs above for the tcp:// addresses.");
        if let Some(b) = &b_addr {
            println!("    bootstrap: {}", b);
        }
        if let Some(d) = &d_addr {
            println!("    data: {}", d);
        }
    }

    // Block until killed.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn run_tunnel_nginx() {
    let bootstrap_port = env_port("PYIELINK_PORT", 4242);
    let data_port = env_port("PYIELINK_DATA_PORT", 4243);
    println!("  [tunnel/nginx] nginx 'stream' reverse-proxy config (run nginx on a PUBLIC VPS):");
    println!("  NOTE: nginx does NOT traverse NAT on its own — the host must be reachable from");
    println!("  the nginx server. For NAT traversal from a private host, use 'cloudflared'.");
    println!();
    println!("  stream {{");
    println!("    server {{ listen {}; proxy_pass 127.0.0.1:{}; }}", bootstrap_port, bootstrap_port);
    println!("    server {{ listen {}; proxy_pass 127.0.0.1:{}; }}", data_port, data_port);
    println!("  }}");
    println!();
    println!("  Client connects to the VPS hostname on port {}.", bootstrap_port);
}