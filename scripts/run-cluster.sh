#!/usr/bin/env bash
# Start a minimal local Brokkr cluster: one control plane + one (no-sandbox)
# worker, for quick experimentation. NOT for production (no TLS, no auth).
#
# Usage:
#   ./scripts/run-cluster.sh            # builds, then runs control + worker
#   BROKKR_LISTEN=127.0.0.1:7878 ./scripts/run-cluster.sh
#
# Then, in another shell:
#   brokk run --control http://127.0.0.1:7878 -- /bin/echo hi
#
# Ctrl-C stops both processes.
set -euo pipefail

LISTEN="${BROKKR_LISTEN:-127.0.0.1:7878}"
DATA_DIR="${BROKKR_DATA_DIR:-./brokkr-data}"
PROFILE_DIR="target/debug"
HA=0
[ "${1:-}" = "--ha" ] && HA=1

echo "==> building workspace binaries"
cargo build -p brokkr-control -p brokkr-worker -p brokkr-cli

CONTROL="$PROFILE_DIR/brokkr-control"
WORKER="$PROFILE_DIR/brokkr-worker"

mkdir -p "$DATA_DIR"

if [ "$HA" = 1 ]; then
  # Phase 5 I9: three Raft-replicated control planes (experimental).
  # Client ports 7878-7880, raft peer ports 7978-7980; the worker attaches
  # to node 0. Kill the leader and watch the survivors take over.
  echo "==> starting 3-node HA control plane (raft, experimental)"
  HA_PIDS=()
  for i in 0 1 2; do
    args=(--listen "127.0.0.1:$((7878 + i))" --data-dir "$DATA_DIR/node$i"
          --raft --node-id "control-$i" --raft-listen "127.0.0.1:$((7978 + i))")
    for j in 0 1 2; do
      [ "$j" = "$i" ] || args+=(--raft-peer "control-$j=127.0.0.1:$((7978 + j))")
    done
    mkdir -p "$DATA_DIR/node$i"
    "$CONTROL" "${args[@]}" &
    HA_PIDS+=($!)
  done
  CONTROL_PID="${HA_PIDS[0]}"
  trap 'kill "${HA_PIDS[@]}" 2>/dev/null || true' EXIT
else
  echo "==> starting control plane on $LISTEN (data: $DATA_DIR)"
  "$CONTROL" --listen "$LISTEN" --data-dir "$DATA_DIR" &
  CONTROL_PID=$!
fi

# Stop both children on exit / Ctrl-C.
cleanup() {
    echo "==> stopping cluster"
    kill "$CONTROL_PID" "${WORKER_PID:-}" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Wait for the control plane to accept connections.
echo "==> waiting for control plane to listen"
for _ in $(seq 1 100); do
    if (exec 3<>"/dev/tcp/${LISTEN%%:*}/${LISTEN##*:}") 2>/dev/null; then
        exec 3>&- 3<&-
        break
    fi
    sleep 0.1
done

echo "==> starting worker (--no-sandbox) against http://$LISTEN"
"$WORKER" --control "http://$LISTEN" --no-sandbox &
WORKER_PID=$!

echo "==> cluster up. control pid=$CONTROL_PID worker pid=$WORKER_PID"
echo "    try: brokk run --control http://$LISTEN -- /bin/echo hi"
echo "    (Ctrl-C to stop)"
wait
