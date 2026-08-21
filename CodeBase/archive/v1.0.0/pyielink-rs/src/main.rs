mod client;
mod creds;
mod host;
mod proto;
mod token;

use std::process::ExitCode;

const DEFAULT_PORT: u16 = 4242;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("  [err] {}", msg);
            ExitCode::from(1)
        }
    }
}

fn usage() -> String {
    format!(
        "pyielink v{} — emtypyie remote access framework\n\n\
         USAGE:\n\
         \x20 pyielink                     interactive launcher\n\
         \x20 pyielink <user>@<ip>         connect to a remote host\n\
         \x20 pyielink /enable [--port N]  open this device for connections (default port {})\n\
         \x20 pyielink /adduser -m <name>  create a local user account (masked password prompt)\n\
         \x20 pyielink -h | --help         this help\n\
         \x20 pyielink -v | --version      version",
        env!("CARGO_PKG_VERSION"),
        DEFAULT_PORT
    )
}

fn run(args: Vec<String>) -> Result<(), String> {
    match args.first().map(|s| s.as_str()) {
        None => launcher(),
        Some("-h" | "--help" | "/help") => {
            println!("{}", usage());
            Ok(())
        }
        Some("-v" | "--version") => {
            println!("pyielink v{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(a) if a.contains('@') => client::run_connect(a),
        Some("/enable") => {
            let port = parse_port(args.get(2))?; // /enable --port N
            cmd_enable_and_listen(port)
        }
        Some("/adduser") => {
            // expected: /adduser -m <name>
            if args.get(1).map(String::as_str) != Some("-m") || args.get(2).is_none() {
                return Err("usage: pyielink /adduser -m <username>".into());
            }
            creds::add_user(&args[2])
        }
        Some(other) => Err(format!("unknown command '{}'\n{}", other, usage())),
    }
}

fn parse_port(arg: Option<&String>) -> Result<u16, String> {
    match arg.map(|s| s.as_str()) {
        None => Ok(DEFAULT_PORT),
        Some(p) => p
            .parse::<u16>()
            .map_err(|_| format!("invalid port '{}'", p)),
    }
}

fn cmd_enable_and_listen(port: u16) -> Result<(), String> {
    creds::cmd_enable()?;
    println!("  [ok] remote access enabled.");
    host::listen(port)
}

/* ---- interactive launcher: the /pyielink entry path (no args forwarded) ---- */

fn launcher() -> Result<(), String> {
    loop {
        println!("\npyielink v{} — emtypyie remote access framework", env!("CARGO_PKG_VERSION"));
        println!("  [1] Connect to a host");
        println!("  [2] Enable remote access on this device");
        println!("  [3] Add a local user account");
        println!("  [0] Exit");
        print!("choice> ");
        use std::io::Write;
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return Ok(());
        }
        match line.trim() {
            "0" | "" => return Ok(()),
            "1" => {
                let target = creds::read_line_prompt("connect as user@ip: ");
                if target.is_empty() {
                    continue;
                }
                if let Err(e) = client::run_connect(&target) {
                    eprintln!("  [err] {}", e);
                }
            }
            "2" => {
                if let Err(e) = cmd_enable_and_listen(DEFAULT_PORT) {
                    eprintln!("  [err] {}", e);
                    continue;
                }
                return Ok(());
            }
            "3" => {
                let name = creds::read_line_prompt("new username: ");
                if name.is_empty() {
                    continue;
                }
                if let Err(e) = creds::add_user(&name) {
                    eprintln!("  [err] {}", e);
                }
            }
            other => println!("  unknown choice '{}'", other),
        }
    }
}
