#!/usr/bin/env bash
# redeploy.sh — build the workspace and (re)deploy captain-miao-server to a
# remote pool host.
#
# Server-side changes (anything the remote daemon/launcher runs) don't take
# effect until the new binary is on the host AND a fresh daemon is running.
# Since the crate split the dashboard can't auto-upload (it no longer links the
# pty pool, so the binary it could send wouldn't be a functional server — see
# the provisioning note in src/backend.rs), and dev builds never bump the
# version, so the connect probe can't tell a stale cache binary from a fresh
# one. This script is therefore the dev deploy loop: kill the remote processes,
# push the freshly built captain-miao-server to the cache path the probe checks
# (~/.cache/captain-miao/bin/captain-miao-server), and let the next dashboard
# connect resolve it via UseCache.
#
# pkill note: a `pkill -f` pattern matches the ssh shell running it (the
# self-match trap); `pkill captain-miao` matches the process *name* instead
# (comm — Linux truncates it to 15 chars, "captain-miao-se", still a substring
# match), so it can't kill its own shell.
#
# Usage:  ./redeploy.sh <host>              # any ssh target (an ~/.ssh/config
#         CAPTAIN_MIAO_DEPLOY_HOST=box \    # host alias, or user@hostname)
#           ./redeploy.sh
# Then quit your dashboard (q) and relaunch it.
set -euo pipefail

HOST="${1:-${CAPTAIN_MIAO_DEPLOY_HOST:-}}"
if [ -z "$HOST" ]; then
    echo "usage: $0 <ssh-host>   (or set CAPTAIN_MIAO_DEPLOY_HOST)" >&2
    exit 2
fi
cd "$(dirname "$0")"

CACHE_BIN=".cache/captain-miao/bin/captain-miao-server"

echo "▶ Building release (workspace: dashboard + server)…"
cargo build --release --workspace

echo "▶ Resetting ${HOST}: clear stale binaries, kill daemon/launchers…"
# ORDER MATTERS: rm the cache binary BEFORE pkill. A connected dashboard
# reconnects within ~500ms of the daemon dying and re-runs `daemon ensure` —
# if the old binary is still at the cache path in that window, it resurrects
# the OLD daemon, whose exe the subsequent rm/upload then replaces under it
# (verified live: `/proc/<pid>/exe -> … (deleted)`, every attach spawn failing
# with "spawn attach --background"). With the binary gone first, reconnect
# attempts fall back to PATH (absent) and keep backing off until the upload
# lands. Both cache-binary names are removed: the pre-split auto-upload left a
# `captain-miao` there. Clearing dead session state files keeps the next
# snapshot clean.
ssh "$HOST" '
  rm -f ~/.cache/captain-miao/bin/captain-miao ~/.cache/captain-miao/bin/captain-miao-server
  pkill captain-miao 2>/dev/null || true
  sleep 1
  rm -f ~/.local/state/captain-miao/sessions/*.json
  mkdir -p ~/.cache/captain-miao/bin
  echo "  ✓ $(hostname) reset"
'

echo "▶ Uploading captain-miao-server…"
# temp + atomic mv so a half-written file is never exec'd.
scp target/release/captain-miao-server "${HOST}:${CACHE_BIN}.tmp"
ssh "$HOST" "mv ~/${CACHE_BIN}.tmp ~/${CACHE_BIN} && chmod +x ~/${CACHE_BIN}"

# The probe matches the dashboard's version against the server's --version, so
# print both for an eyeball check (a mismatch degrades to FallBack/PATH).
echo "  local dashboard: $(./target/release/captain-miao --version)"
echo "  remote server:   $(ssh "$HOST" "~/${CACHE_BIN} --version")"

echo "▶ Done. Now quit your dashboard (q) and relaunch it:"
echo "    CAPTAIN_MIAO_DEBUG=1 ./target/release/captain-miao"
echo "  The provision log should show '→ UseCache' and a fresh daemon starting."
