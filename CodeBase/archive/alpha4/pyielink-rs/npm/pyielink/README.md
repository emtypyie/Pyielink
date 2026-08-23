# Pyielink

**Pyielink v0.5.2** — Remote access framework for screen sharing, file transfer, and terminal access.

## Installation

```bash
# Install globally via npm (or emtypyie.cli)
npm install -g pyielink

# or via emtypyie.cli:
emtypyie /get pyielink
```

The npm package is fully self-contained:

- **`bin/pyielink(.exe)`** — the prebuilt Rust binary, bundled with the package
- **`datalayer/`** — the Node.js data-layer (per-session WebSocket multiplexer) used by the Rust binary
- **`cli.js`** — the wrapper that runs the binary with the right environment

No separate download step is required — everything ships in the package.

## Usage

### Connect to a host

```bash
# GUI mode (default)
pyielink user@192.168.1.100

# REPL terminal mode
pyielink --repl user@192.168.1.100
```

### Enable host (run on the host machine)

```bash
# Enable for any IP (open access)
pyielink enable --all

# Enable for specific IP only
pyielink enable --whitelist 192.168.1.100

# Add IP to whitelist (persistent)
pyielink whitelist add 10.0.0.1

# Remove IP from whitelist
pyielink whitelist remove 10.0.0.1
```

On first successful connection a credential token is stored under
`~/.pyielink/tokens/` (Windows: `%APPDATA%\.pyielink\tokens\`).

## Environment Variables

The wrapper sets these automatically; you normally don't need to touch them:

| Variable             | Purpose                                                      |
| -------------------- | ------------------------------------------------------------ |
| `PYIELINK_HOME`      | Persistent state dir (`~/.pyielink` / `%APPDATA%\.pyielink`) |
| `PYIELINK_DATALAYER` | Path to the bundled `datalayer/` Node.js component           |

If the bundled data layer is missing, set `PYIELINK_DATALAYER` to the
directory that contains `src/server.js`.

## How It Works

- The **npm package** (`pyielink`) bundles the Rust binary and the Node.js data layer.
- **`cli.js`** resolves the package, sets `PYIELINK_HOME` / `PYIELINK_DATALAYER`, and runs the Rust binary.
- The Rust binary handles the connection, then spawns the **`datalayer`** Node server for file-transfer / screen-sharing channels.
- Tokens and the IP whitelist persist in `~/.pyielink/`.

Features: screen sharing, file transfer, audio, multi-monitor, and tunneling.

## Requirements

- **Node.js >= 18** (required to run the data layer)
- A network route to the host (or a tunnel)

## License

MIT

## Repository

[https://github.com/EMTYPYIE/Pyielink](https://github.com/EMTYPYIE/Pyielink)
