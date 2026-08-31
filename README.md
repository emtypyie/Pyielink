<img width="1280" height="640" alt="pyielinkbanner" src="https://github.com/user-attachments/assets/7bbcf92f-54eb-43ea-b6fa-67f83c0af36c" />

# PYIELINK FRAMEWORK

PYIELINK FRAMEWORK is a remote machine access framework with peer-to-peer connection over encrypted internet.

## Install

PYIELINK FRAMEWORK is distributed as the `pyielink` CLI. Requires **Node.js 18+** and `ffmpeg` / `ffplay` on `PATH`.

### npm
```sh
npm install -g pyielink
```

### emtypyie.cli
Requires **emtypyie.cli v3.5.x or newer**. From https://emtypyie.in/cli, run:
```
/get pyielink
```
Once installed it runs as `/pyielink` inside the CLI (the shell does not forward arguments).

After install, `pyielink` (or `/pyielink`) is on your `PATH` — see Commands below.

## Commands

| Command | What it does |
|---|---|
| `pyielink` | Show usage / help (no arguments). |
| `pyielink <user>@<ip>` | Connect to a peer. After authentication the data layer starts, video transmission begins, and the GUI window opens. |
| `pyielink <user>@<ip> --repl` | Connect and open an interactive shell (like `ssh user@ip` — same terminal access, written in Rust). Also accepts `pyielink --repl <user>@<ip>`. |
| `pyielink enable` | Enable this device for connections and start the listener. |
| `pyielink enable --all` | Enable for connections from any IP. |
| `pyielink enable --whitelist <IP>` | Enable, allowing connections only from the given IP. |
| `pyielink adduser -m "<name>"` | Create a host user account (prompts for password). |
| `pyielink adduser -m "<name>" -r "<role>"` | Create an account with `role` `user` or `admin`. |
| `pyielink whitelist add <IP>` | Add an IP to the connection whitelist. |
| `pyielink whitelist remove <IP>` | Remove an IP from the connection whitelist. |
| `pyielink tunnel start` | Start a tunnel (`cloudflared` or `nginx`; requires the binary). |
| `pyielink host` | Start the host listener and accept incoming connections (port 4242). |
| `pyielink -h` / `--help` | Show help. |
| `pyielink -v` / `--version` | Show version. |

## Docker

PYIELINK FRAMEWORK ships a single container image that runs either the **host**
(shares a screen) or the **viewer** (watches a remote screen).

```sh
cd CodeBase/v0.7.0/pyielink-rs
# edit pyielink.conf (set PYIELINK_HOST=<host-ip> for the viewer)
docker compose build
docker compose up -d host
docker compose run --rm view
```

Settings come from the explicit `pyielink.conf` file (no hidden `.env` file).
You can also override any value per run, e.g.:
`docker compose run --rm -e PYIELINK_HOST=<host-ip> view`.

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
