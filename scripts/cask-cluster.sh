#!/usr/bin/env bash
# Starts a throwaway local Cask cluster and runs a command with RADIANT_TEST_CASK_URLS set to
# its nodes. The cluster is stopped and its data deleted when the command exits.
# Needs `cask` on PATH (it is in the Nix dev shell).
#
#   nix develop --command scripts/cask-cluster.sh [-n NODES] -- COMMAND...
set -euo pipefail

nodes=3
if [[ "${1:-}" == "-n" ]]; then nodes=$2; shift 2; fi
[[ "${1:-}" == "--" ]] && shift
[[ $# -gt 0 ]] || { echo "usage: $0 [-n NODES] -- COMMAND..." >&2; exit 2; }

workdir=$(mktemp -d)
base=$(( 21000 + (RANDOM % 400) * 10 ))
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$workdir"
}
trap cleanup EXIT

urls=()
for i in $(seq 0 $((nodes - 1))); do
  port=$((base + i)) gossip=$((base + 100 + i))
  neighbors=""
  for j in $(seq 0 $((nodes - 1))); do
    [[ $j -ne $i ]] && neighbors+="${neighbors:+,}127.0.0.1:$((base + 100 + j))"
  done
  mkdir -p "$workdir/node$i"
  env CASK_PORT=$port CASK_GOSSIP_PORT=$gossip CASK_BASE_URL="http://127.0.0.1:$port" \
    CASK_UUID="$(printf '%08d-0000-4000-8000-%012d' "$i" "$base")" \
    CASK_WRITEABLE=true CASK_BACKEND=disk CASK_DISK_BACKEND_ROOT="$workdir/node$i/" \
    CASK_NEIGHBORS="$neighbors" CASK_REPLICATION=2 CASK_MAX_REPLICATION=3 \
    CASK_CLUSTER_SECRET=radiant-test CASK_HEARTBEAT_INTERVAL=1 CASK_AAE_INTERVAL=3600 \
    CASK_KEEP_FREE=1 \
    cask >"$workdir/node$i.log" 2>&1 &
  pids+=($!)
  echo $! >>"$workdir/pids"
  urls+=("http://127.0.0.1:$port")
done

for u in "${urls[@]}"; do
  for _ in $(seq 1 50); do curl -sf -o /dev/null "$u/config/" && break; sleep 0.2; done
  curl -sf -o /dev/null "$u/config/" || { echo "cask node $u did not start" >&2; cat "$workdir"/node*.log >&2; exit 1; }
done
# Let gossip settle so writes replicate.
sleep 2

export RADIANT_TEST_CASK_URLS
RADIANT_TEST_CASK_URLS=$(IFS=,; echo "${urls[*]}")
# Logs, data and a `pids` file (one node per line, in URL order) live here.
export CASK_TEST_DIR="$workdir"
"$@"
