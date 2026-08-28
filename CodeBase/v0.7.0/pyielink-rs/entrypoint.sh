#!/bin/sh
# PYIELINK FRAMEWORK container entrypoint
# Mode is the first argument: host | view  (defaults to host).
#
# host: starts the data layer that captures + streams this machine's screen.
# view: connects to a host and opens the screen in ffplay (Xvfb if headless).
#
# A session key is shared between the two via the `pyielink-data` volume
# (/data/host_key.txt) or the PYIELINK_KEY env var.

set -e
MODE="${1:-host}"
DATA=/data
PORT="${PYIELINK_DL_PORT:-4243}"

gen_key() {
    if [ -z "$PYIELINK_KEY" ] && [ -f "$DATA/host_key.txt" ]; then
        PYIELINK_KEY=$(cat "$DATA/host_key.txt")
    fi
    if [ -z "$PYIELINK_KEY" ]; then
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
        echo "[pyielink] streaming on ws://0.0.0.0:${PORT}"
        PYIELINK_SESSION="$HANDOFF" PYIELINK_DL_PORT="$PORT" \
            exec node /app/datalayer/src/server.js --port "$PORT"
        ;;
    view)
        KEY=$(gen_key)
        : "${PYIELINK_HOST:?set PYIELINK_HOST=<host-ip> (env or .env)}"
        if [ -z "$DISPLAY" ]; then
            Xvfb :99 -screen 0 1280x720x24 >/dev/null 2>&1 &
            export DISPLAY=:99
        fi
        echo "[pyielink] VIEW mode — connecting to $PYIELINK_HOST:${PORT}"
        exec node /app/datalayer/src/client_view.js \
            --host "$PYIELINK_HOST" --port "$PORT" --key "$KEY"
        ;;
    *)
        echo "usage: entrypoint.sh [host|view]" >&2
        exit 1
        ;;
esac
