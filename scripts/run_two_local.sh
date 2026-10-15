#!/usr/bin/env bash
# Run two local LanDrop instances for testing:
#   A on port 8420, B on port 8421 — each with an isolated config + download
#   dir under a per-run temp directory (cleaned up on exit).
#
# mDNS rarely works on loopback, so the script also registers each instance as
# the other's manual peer before printing the URLs.
set -euo pipefail

cd "$(dirname "$0")/.."

PORT_A=8420
PORT_B=8421
RUN_DIR="$(mktemp -d -t landrop-two-XXXXXX)"
DIR_A="$RUN_DIR/a-downloads"
DIR_B="$RUN_DIR/b-downloads"
CFG_A="$RUN_DIR/a-config"
CFG_B="$RUN_DIR/b-config"
LOG_A="$RUN_DIR/a.log"
LOG_B="$RUN_DIR/b.log"

mkdir -p "$DIR_A" "$DIR_B" "$CFG_A" "$CFG_B"

cleanup() {
  if [[ -n "${PID_A:-}" ]]; then kill "$PID_A" 2>/dev/null || true; fi
  if [[ -n "${PID_B:-}" ]]; then kill "$PID_B" 2>/dev/null || true; fi
  rm -rf "$RUN_DIR"
}
trap cleanup EXIT

echo "==> Building landrop (release)"
cargo build --release

echo "==> Starting instance A on port $PORT_A"
./target/release/landrop \
  --port "$PORT_A" --no-port-scan \
  --downloads "$DIR_A" \
  --config-dir "$CFG_A" \
  --name landrop-a \
  >"$LOG_A" 2>&1 &
PID_A=$!

echo "==> Starting instance B on port $PORT_B"
./target/release/landrop \
  --port "$PORT_B" --no-port-scan \
  --downloads "$DIR_B" \
  --config-dir "$CFG_B" \
  --name landrop-b \
  >"$LOG_B" 2>&1 &
PID_B=$!

# Wait until a node answers /api/info (bounded), failing loudly otherwise.
wait_ready() {
  local port="$1" pid="$2" log="$3"
  for _ in $(seq 1 50); do
    if curl -fsS "http://127.0.0.1:$port/api/info" >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "instance on port $port exited early; log ($log):" >&2
      sed -n '1,40p' "$log" >&2
      return 1
    fi
    sleep 0.1
  done
  echo "instance on port $port did not become ready; log ($log):" >&2
  sed -n '1,40p' "$log" >&2
  return 1
}

wait_ready "$PORT_A" "$PID_A" "$LOG_A"
wait_ready "$PORT_B" "$PID_B" "$LOG_B"

# Register each instance with the other as a manual peer.
curl -fsS -X POST "http://127.0.0.1:$PORT_A/api/peers/manual" \
  -H 'Content-Type: application/json' \
  -d "{\"address\":\"127.0.0.1:$PORT_B\"}" >/dev/null
curl -fsS -X POST "http://127.0.0.1:$PORT_B/api/peers/manual" \
  -H 'Content-Type: application/json' \
  -d "{\"address\":\"127.0.0.1:$PORT_A\"}" >/dev/null

echo ""
echo "============================================"
echo "  LanDrop A: http://localhost:$PORT_A"
echo "  LanDrop B: http://localhost:$PORT_B"
echo "  downloads: $DIR_A  /  $DIR_B"
echo "============================================"
echo ""
echo "Each instance already sees the other as a manual peer."
echo "Send a file from A to B, then accept it in B."
echo ""
echo "Press Ctrl-C to stop both (temp state is cleaned up)."

# Stop both when either exits, so a crashed instance is noticed immediately.
wait -n "$PID_A" "$PID_B" || true
echo "an instance exited; shutting the other one down"
