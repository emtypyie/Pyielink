#!/bin/sh
# pyielink connector entrypoint: never give up.
# Reconnects forever with a fixed backoff so transient host/network
# problems, crashes, or reboots never permanently stop the client.

TARGET="${PYIELINK_TARGET:-}"
if [ -z "$TARGET" ]; then
    echo "[entrypoint] PYIELINK_TARGET=user@ip is required (compose reads it from .env)"
    exit 1
fi

DELAY="${PYIELINK_RETRY_SECONDS:-5}"
echo "[entrypoint] connector starting: target=${TARGET} retry=${DELAY}s"

# interactive bootstrap (license + password) only when a TTY is attached;
# once the token file exists in /data/tokens every later pass is zero-input
while true; do
    PYIELINK_SHELL="${PYIELINK_SHELL:-1}" pyielink "$TARGET"
    CODE=$?
    echo "[entrypoint] session ended (exit ${CODE}) — retrying in ${DELAY}s"
    sleep "$DELAY"
done
