use std::env;
use std::process;

use pyielink::creds::{add_to_whitelist, remove_from_whitelist, add_user, cmd_enable_with_flags};
use pyielink::tunnel::{TunnelManager, tunnel_manager, TunnelType};
use pyielink::RunMode;

fn print_usage() {
    eprintln!("usage: pyielink <command> [args...]");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  user@ip                   Connect to host (GUI mode)");
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

        "--repl" => {
            if args.len() < 2 {
                eprintln!("  [error] usage: pyielink --repl user@ip");
                process::exit(1);
            }
            let target = &args[1];
            if let Err(e) = pyielink::client::run_connect(target, true) {
                eprintln!("  [error] connection failed: {}", e);
                process::exit(1);
            }
        }

        "tunnel" => {
            if args.len() < 2 {
                eprintln!("  [error] usage: pyielink tunnel start [cloudflared|nginx]");
                process::exit(1);
            }

            let subcommand = &args[1];
            match subcommand.as_str() {
                "start" => {
                    // Show info about tunnel options
                    eprintln!("  [info] Tunnel types:");
                    eprintln!("    cloudflared - requires cloudflared binary (https://cloudflare.com/cloudflared/)");
                    eprintln!("    nginx       - reverse proxy (nginx must be installed)");
                    eprintln!();
                    eprintln!("  Usage: pyielink tunnel start <type>");
                    eprintln!("  Example: pyielink tunnel start cloudflared");
                }
                _ => {
                    eprintln!("  [error] unknown tunnel subcommand: {}", subcommand);
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