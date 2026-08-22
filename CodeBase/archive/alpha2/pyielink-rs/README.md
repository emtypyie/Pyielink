# pyielink-rs — Rust Bootstrap Layer (v1.0.1)

Single-binary bootstrap for the pyielink framework: CLI parsing, host
provisioning, license handshake, challenge-response authentication,
connection-token issuance, and session heartbeats.

## Commands

| Command | Purpose |
|---|---|
| `pyielink` | Interactive launcher (this is what emtypyie.cli's `/pyielink` runs — the shell does not forward arguments) |
| `pyielink <user>@<ip>` | Connect to a remote host |
| `pyielink /enable [--port N]` | Open this device for connections and start the listener (default port 4242) |
| `pyielink /adduser -m <name>` | Create a local account; prompts create-password then confirm-password |
| `pyielink -h` / `-v` | Help / version |

## State

- Host: `~/.pyielink/host_state.json` (`enabled` flag, per-user salted password hash, licensed flag, token hash). Only hashes are stored — never raw passwords or tokens.
- Client: `~/.pyielink/tokens/<user>@<ip>` — stores `sha256(token)`, never the raw token.

## Wire protocol

TCP frames: `[u8 type][u16 len BE][payload]`, 64 KiB max payload.

| Type | Name | Direction | Payload |
|---|---|---|---|
| `0x01` | HELLO | C→H | `<user>\n<client_ver>` |
| `0x0B` | CHALLENGE | H→C | `<pw_salt>\n<nonce>` (64-hex nonce, fresh per attempt) |
| `0x0C` | PROOF | C→H | `t:<hex>` or `p:<hex>` where hex = `sha256(secret ‖ nonce)` |
| `0x04`/`0x05`/`0x06` | LICENSE_TEXT / ACCEPT / REJECT | H↔C | agreement body / `y` / `n` |
| `0x07` | TOKEN_ISSUED | H→C | raw token (once, over the authenticated channel) |
| `0x09` | AUTH_OK | H→C | promotion ticket: `<data_port>\n<session_key>` |
| `0x0A` | AUTH_FAIL | H→C | reason string |
| `0x0D`/`0x0E` | PING / PONG | H→C / C→H | timestamped payload echoed back |
| `0x0F` | BYE | either | empty — clean shutdown |

Secrets never cross the wire: the client proves knowledge of the stored
password hash (derived locally from the salt in the challenge) or of the
stored token hash. Nonces make proofs single-use; per-IP throttling allows
5 failures per 60 s before lockout.

## Build & test

```sh
cargo build --release        # -> target/release/pyielink.exe
cargo test                   # 22 unit tests (codec, hashing, proofs, JSON, tokens, sessions)
```

## Milestone suite

End-to-end localhost scenario driving the real binaries:

```sh
powershell -ExecutionPolicy Bypass -File test-milestone.ps1
# optionally: -Exe target\release\pyielink.exe
```

Covers: provisioning + duplicate rejection, disabled-host refusal,
unknown-user refusal, wrong-proof retries then success, license
accept/reject, token issuance/rotation/token-only re-auth via proofs,
fresh-nonce enforcement, heartbeat PING/PONG round-trips + clean BYE,
auth exhaustion, malformed-frame survival, real-client zero-input token
reconnect with live heartbeats, and per-IP lockout after 5 consecutive
failures.

## Module map

- `main.rs` — arg dispatch + interactive launcher
- `proto.rs` — frame codec
- `creds.rs` — host state JSON, iterated-SHA-256 password hashing, nonce/proof helpers, per-IP fail throttle, masked input (console `_getch`, pipe-safe fallback)
- `token.rs` — CSPRNG tokens, hex, client token storage (hash only)
- `sessions.rs` — TTL'd session-key registry for the Phase 2 data port
- `host.rs` — `/enable` listener, challenge-response handshakes, license gate, token rotation, heartbeat loop
- `client.rs` — connect flow, proof answering, license prompt, ticket parsing, heartbeat responder

## Known MVP limitations (see plan.md §7 hardening)

- Plaintext transport — TLS before any non-LAN use
- Iterated SHA-256 instead of Argon2id
- Proof scheme is HMAC-less SHA-256(secret‖nonce); fine for LAN MVP, revisit before exposure
