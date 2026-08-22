# pyielink-rs — Rust Bootstrap Layer

Version: **0.1.0-alpha.4** (development snapshot; alpha3 is the tagged
bootstrap release — data layer work happens in this tree).

Single-binary bootstrap for the pyielink framework: CLI parsing, host
provisioning, license handshake, challenge-response authentication,
connection-token issuance, role-based remote terminal, and session
heartbeats. Ships with a resilient Docker connector.

## Commands

| Command | Purpose |
|---|---|
| `pyielink` | Interactive launcher (this is what emtypyie.cli's `/pyielink` runs — the shell does not forward arguments) |
| `pyielink <user>@<ip>` | Connect to a remote host; after promotion you get an interactive remote terminal |
| `pyielink /enable [--port N]` | Open this device for connections and start the listener; prints all local IPs clients can target |
| `pyielink /adduser -m <name> [-r user\|admin]` | Create a local account; admins may run `sudo` commands remotely |
| `pyielink -h` / `-v` | Help / version |

## Remote terminal & roles

Once promoted (`AUTH_OK`), interactive sessions get a command prompt:

- type any command — it runs on the host, stdout/stderr stream back live,
  and the exit code is reported when it finishes
- `sudo <command>` requests elevation: **only accounts with the `admin`
  role are granted it**; standard users receive `EXEC_DENY`
- `exit` / Ctrl-D sends BYE and closes cleanly

Notes: one command runs at a time per session; heartbeats continue while a
command executes (long-running output is capped at 512 KiB); elevation
currently means "runs with the host process's privileges" — UAC-integrated
elevation is backlog. The terminal channel multiplexes with heartbeats on
the same socket using frames `EXEC_REQ/OUT/END/DENY` (0x10–0x13).

## State — encrypted at rest, nothing hardcoded

There are **no default or baked-in users**; every account exists only
because someone ran `/adduser` on that machine. The state file
`~/.pyielink/host_state.json` is sealed with AES-256-GCM under a random
32-byte master key in `~/.pyielink/host.key`:

```
PYLENC1:<hex nonce><hex ciphertext+tag>
```

- plaintext passwords/tokens are never stored anywhere (only iterated
  SHA-256 password hashes and SHA-256 token hashes inside the sealed blob)
- legacy plaintext state files still load and are re-sealed on next save
- losing `host.key` means accounts must be recreated — no recovery path,
  by design
- `PYIELINK_PLAINTEXT_STATE=1` disables sealing (CI/test escape hatch
  only — never on real deployments)

## Wire protocol

TCP frames: `[u8 type][u16 len BE][payload]`, 64 KiB max payload.

| Type | Name | Direction | Payload |
|---|---|---|---|
| `0x01` | HELLO | C→H | `<user>\n<client_ver>` |
| `0x0B` | CHALLENGE | H→C | `<pw_salt>\n<nonce>` (fresh per attempt) |
| `0x0C` | PROOF | C→H | `t:<hex>` or `p:<hex>`; hex = `sha256(secret ‖ nonce)` |
| `0x04`/`0x05`/`0x06` | LICENSE_TEXT / ACCEPT / REJECT | H↔C | agreement body / `y` / `n` |
| `0x07` | TOKEN_ISSUED | H→C | raw token (once, over the authenticated channel) |
| `0x09` | AUTH_OK | H→C | `<data_port>\n<session_key>` promotion ticket |
| `0x0A` | AUTH_FAIL | H→C | reason string |
| `0x0D`/`0x0E` | PING / PONG | both | timestamped payload echoed back |
| `0x10` | EXEC_REQ | C→H | `[flag '0'\|'1'][command]` (flag '1' = sudo request) |
| `0x11` | EXEC_OUT | H→C | raw stdout/stderr chunk |
| `0x12` | EXEC_END | H→C | exit code as ASCII decimal |
| `0x13` | EXEC_DENY | H→C | reason string |
| `0x0F` | BYE | either | empty — clean shutdown |

Secrets never cross the wire; nonces make proofs single-use; per-source-IP
throttling locks out after 5 consecutive failures for 60 s (any success
clears the counter).

## Docker connector (never stops)

```sh
cd CodeBase/archive/v1.1.0/pyielink-rs

# one-time interactive bootstrap (accept license, type password once):
docker compose run --rm connector          # PYIELINK_TARGET comes from .env

# then run as a self-healing daemon:
PYIELINK_TARGET=bob@192.168.1.50 docker compose up -d

# attach to the live remote terminal:
docker attach $(docker compose ps -q connector)
```

Resilience stack-up so the client "doesn't stop for any problem":

1. in-binary heartbeat watchdog detects dead hosts within ~20 s
2. `entrypoint.sh` retries forever with backoff (`PYIELINK_RETRY_SECONDS`)
3. `restart: unless-stopped` survives crashes AND daemon reboots
4. credential volume `connector-data` survives container recreation

Set `PYIELINK_ACCEPT_LICENSE=1` to pre-accept the ethics agreement for
headless first runs — setting it is the acceptance.

## Build & test

```sh
cargo build --release        # -> target/release/pyielink.exe
cargo test                   # 25 unit tests (codec, hashing, proofs, AES-GCM seal, roles, tokens, sessions)
```

## Milestone suite

```sh
powershell -ExecutionPolicy Bypass -File test-milestone.ps1
```

Covers: encrypted-at-rest state (default mode), provisioning + roles +
duplicate rejection, disabled-host refusal, unknown-user refusal, wrong-
proof retries then success, license accept/reject, token issuance/rotation/
token-only re-auth via proofs, fresh-nonce enforcement, heartbeats + clean
BYE, auth exhaustion, malformed-frame survival, real-client zero-input
reconnect with heartbeats, IP banner, sudo denial for standard users,
elevated execution for admins, and per-IP lockout.

## Module map

- `main.rs` — arg dispatch + interactive launcher
- `proto.rs` — frame codec
- `creds.rs` — encrypted state (AES-256-GCM + keyfile), roles, iterated-SHA-256 password hashing, nonce/proof helpers, per-IP fail throttle, masked input
- `token.rs` — CSPRNG tokens/hex, client token storage (hash only), PYIELINK_HOME path root
- `sessions.rs` — TTL'd session-key registry for the Phase 2 data port
- `host.rs` — `/enable` listener + IP banner, challenge-response handshakes, license gate, token rotation, session loop (heartbeats ∥ exec channel)
- `client.rs` — connect flow (+IP printout), proof answering, license prompt (env pre-accept), interactive remote terminal / passive monitor

## Known MVP limitations (see plan.md §7 hardening)

- Plaintext transport — TLS before any non-LAN use
- Iterated SHA-256 instead of Argon2id
- Elevation = host-process privileges; true UAC integration pending
- Docker image not yet built in CI (Dockerfile verified by inspection)
