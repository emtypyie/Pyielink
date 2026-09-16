#!/bin/sh
# PYIELINK FRAMEWORK container entrypoint
# Mode is the first argument: host | client  (defaults to host).
#
# host: starts the data layer (file/input services; video/audio gated by
#       PYIELINK_MEDIA=1).
# client: connects to a host and opens the interactive terminal + file
#       explorer REPL (native Rust client; a desktop GUI is no longer shipped).
#
# Settings are read from the mounted explicit config file /app/pyielink.conf,
# then overridden by any matching environment variables (e.g. -e PYIELINK_HOST).
# A session key is shared between the two via the `pyielink-data` volume
# (/data/host_key.txt), the PYIELINK_KEY config value, or the env var.

set -e
MODE="${1:-host}"
DATA=/data

# Load the explicit config file if present (a plain KEY=VALUE list).
if [ -f /app/pyielink.conf ]; then
    set -a
    # shellcheck disable=SC1091
    . /app/pyielink.conf
    set +a
fi

PORT="${PYIELINK_DL_PORT:-4243}"

gen_key() {
    if [ -z "${PYIELINK_KEY:-}" ] && [ -f "$DATA/host_key.txt" ]; then
        PYIELINK_KEY=$(cat "$DATA/host_key.txt")
    fi
    if [ -z "${PYIELINK_KEY:-}" ]; then
        PYIELINK_KEY=$(head -c 16 /dev/urandom | xxd -p)
        echo "$PYIELINK_KEY" > "$DATA/host_key.txt"
    fi
    echo "$PYIELINK_KEY"
}

case "$MODE" in
    host)
        KEY=$(gen_key)
        HANDOFF=$(mktemp)
        printf '%s\nuser\nuser\n' "$KEY" > "$HANDOFF"
        echo "[pyielink] HOST mode — session key: $KEY"
        echo "[pyielink] data layer on ws://0.0.0.0:${PORT}"
        PYIELINK_SESSION="$HANDOFF" PYIELINK_DL_PORT="$PORT" \
            exec node /app/datalayer/src/server.js --port "$PORT"
        ;;
    client)
        : "${PYIELINK_HOST:?set PYIELINK_HOST=<host-ip> in pyielink.conf or pass -e PYIELINK_HOST=<host-ip> client}"
        echo "[pyielink] CLIENT mode — connecting to $PYIELINK_HOST"
        exec /app/pyielink "$PYIELINK_USER@$PYIELINK_HOST"
        ;;
    *)
        echo "usage: entrypoint.sh [host|client]" >&2
        exit 1
        ;;
esac
