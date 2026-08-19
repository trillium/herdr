#!/bin/bash
# Verify federation input relay round-trip:
# 1. Ensure fed-hub is running with the debug binary
# 2. Pick a live foreign pane (first mini2 pane)
# 3. Send a unique marker via hub relay
# 4. Read back via hub pane.read; confirm marker appears within timeout
# Exit 0 = relay working. Exit 1 = relay broken.

set -euo pipefail

BINARY="/Users/trilliumsmith/code/herdr-oss/target/debug/herdr"
SESSION="fed-hub"
MARKER="FED_RELAY_$(date +%s)"
TIMEOUT=15
POLL_INTERVAL=0.5

herdr() {
  env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH "$BINARY" --session "$SESSION" "$@"
}

# ── 1. Start hub if not running ────────────────────────────────────────────────
SOCK="$HOME/.config/herdr-dev/sessions/$SESSION/herdr.sock"
if ! [ -S "$SOCK" ]; then
  echo "Starting $SESSION..."
  env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH "$BINARY" --session "$SESSION" server \
    > "$HOME/.config/herdr-dev/sessions/$SESSION/herdr-server.log" 2>&1 &
  for i in $(seq 1 40); do [ -S "$SOCK" ] && break; sleep 0.25; done
  [ -S "$SOCK" ] || { echo "FAIL: hub did not start"; exit 1; }
  echo "Hub started. Waiting 8s for federation poll..."
  sleep 8
else
  echo "Hub already running."
fi

# ── 2. Find a live foreign pane (mini2 preferred) ─────────────────────────────
PANE_ID=$(herdr pane list 2>/dev/null | python3 -c "
import sys, json
data = json.load(sys.stdin)
for p in data.get('result', {}).get('panes', []):
    ws = p.get('workspace_id', '')
    if 'nBtWpAUbjx11' in ws:   # mini2
        print(p['pane_id']); sys.exit(0)
# fallback: any foreign pane
for p in data.get('result', {}).get('panes', []):
    if p.get('pane_id','').startswith('fed~'):
        print(p['pane_id']); sys.exit(0)
sys.exit(1)
" 2>/dev/null) || { echo "FAIL: no foreign pane found (federation not polling yet?)"; exit 1; }

echo "Target pane: $PANE_ID"
echo "Marker:      $MARKER"

# ── 3. Send marker via hub relay ──────────────────────────────────────────────
herdr pane send-text "$PANE_ID" "echo $MARKER"
echo "Sent. Polling for marker (timeout ${TIMEOUT}s)..."

# ── 4. Poll pane read until marker appears ────────────────────────────────────
DEADLINE=$(( $(date +%s) + TIMEOUT ))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  CONTENT=$(herdr pane read "$PANE_ID" --source visible --lines 40 2>/dev/null || true)
  if echo "$CONTENT" | grep -q "$MARKER"; then
    echo "PASS: marker found in remote pane."
    echo ""
    echo "Connect with:"
    echo "  env -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH $BINARY --session $SESSION"
    exit 0
  fi
  sleep "$POLL_INTERVAL"
done

echo "FAIL: marker '$MARKER' not found in pane after ${TIMEOUT}s."
echo "Last visible content:"
herdr pane read "$PANE_ID" --source visible --lines 10 2>/dev/null || true
exit 1
