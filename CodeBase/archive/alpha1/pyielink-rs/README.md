# pyielink-rs — Rust Bootstrap Layer (v1.0.0)

Single-binary bootstrap for the pyielink framework: CLI parsing, host
provisioning, license handshake, and connection-token issuance.

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
- Client: `~/.pyielink/tokens/<user>@<ip>` (raw token, chmod-equivalent kept simple for MVP).

## Wire protocol

TCP frames: `[u8 type][u16 len BE][payload]`, 64 KiB max payload.
Types `0x01`–`0x0A`: HELLO, PASSWD_REQ, PASSWD_AUTH, LICENSE_TEXT,
LICENSE_ACCEPT, LICENSE_REJECT, TOKEN_ISSUED, AUTH_TOKEN, AUTH_OK, AUTH_FAIL.
See `plan.md` §2 for full flows.

## Build & test

```sh
cargo build --release        # -> target/release/pyielink.exe (~265 KB)
cargo test                   # 14 unit tests (codec, hashing, JSON, tokens)
```

## Milestone suite

End-to-end localhost scenario driving the real binaries:

```sh
powershell -ExecutionPolicy Bypass -File test-milestone.ps1
# optionally: -Exe target\release\pyielink.exe
```

Covers: provisioning + duplicate rejection, disabled-host refusal,
unknown-user refusal, wrong-password retries, license accept/reject,
token issuance/rotation/token-only re-auth, auth exhaustion,
malformed-frame survival, and both real-client paths (password, then
zero-input token reconnect).

## Module map

- `main.rs` — arg dispatch + interactive launcher
- `proto.rs` — frame codec
- `creds.rs` — host state JSON, iterated-SHA-256 password hashing, masked input (console `_getch`, pipe-safe fallback)
- `token.rs` — CSPRNG tokens, hex, client token storage
- `host.rs` — `/enable` listener + first-time/returning handshakes
- `client.rs` — connect flow, license prompt, token persistence

## Known MVP limitations (see plan.md §7 hardening)

- Plaintext transport — TLS before any non-LAN use
- Iterated SHA-256 instead of Argon2id
- No rate limiting beyond 3 attempts per connection
- IPv4-style targets assumed for token filenames
