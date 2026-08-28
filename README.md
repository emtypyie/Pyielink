<img width="1280" height="640" alt="pyielinkbanner" src="https://github.com/user-attachments/assets/7bbcf92f-54eb-43ea-b6fa-67f83c0af36c" />

# PYIELINK FRAMEWORK

PYIELINK FRAMEWORK is a remote machine access framework with peer-to-peer connection over encrypted internet.

## Install

- **npm:** `npm install -g pyielink`
- **emtypyie.cli:** `/get pyielink` from https://emtypyie.in/cli — once installed it runs as `/pyielink` inside the CLI (the shell does not forward arguments)

## Commands

| Command | What it does |
|---|---|
| `pyielink` | Interactive launcher. |
| `pyielink <user>@<ip>` | Connect to a peer. After authentication the data layer starts, video transmission begins, and the GUI window opens. |
| `pyielink <user>@<ip> --repl` | Connect and open an interactive shell (like `ssh user@ip` — same terminal access, written in Rust; see v0.6.0 for reference). |
| `pyielink enable [--port N]` | Open this device for connections and start the listener; prints the local IPs clients can target. |
| `pyielink host` | Start the host / data layer. |
| `pyielink adduser -m "<name>" -r "<role>"` | Create a local account (`role` is `user` or `admin`). |
| `pyielink -h` | Show help. |
| `pyielink -v` | Show version. |

## Docker

PYIELINK FRAMEWORK ships a single container image that runs either the **host**
(shares a screen) or the **viewer** (watches a remote screen).

```sh
cd CodeBase/v0.7.0/pyielink-rs
cp .env.example .env      # set PYIELINK_HOST=<host-ip>
docker compose build
docker compose up -d host
docker compose run --rm view
```

The session key is generated once and shared via the `pyielink-data` volume.
On headless viewers the entrypoint starts `Xvfb` automatically; host screen
capture uses DXGI on Windows (Linux x11grab capture is TODO).

COPYRIGHT EMTYPYIE 2026
DESIGNED AND ENGINEERED BY EMTYPYIE&CO

## License

Copyright © 2026 EMTYPYIE. All rights reserved.

PYIELINK FRAMEWORK is proprietary software designed and engineered by EMTYPYIE&CO.
You may not copy, modify, distribute, decompile, or use this software, in
whole or in part, except as expressly permitted in writing by EMTYPYIE.
For licensing inquiries, contact EMTYPYIE.
