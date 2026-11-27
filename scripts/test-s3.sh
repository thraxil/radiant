#!/usr/bin/env bash
# Runs the S3 tests against a throwaway S3-compatible server (versitygw serving a temp
# directory). Needs `versitygw` and `curl` on PATH; both are in the Nix dev shell. Extra
# arguments are passed to `cargo test`.
#
#   nix develop --command scripts/test-s3.sh
set -euo pipefail

workdir=$(mktemp -d)
port=$(( 20000 + RANDOM % 20000 ))
access=radiant-test
secret=radiant-test-secret
endpoint="http://127.0.0.1:$port"

cleanup() {
  if [[ -n "${gw_pid:-}" ]]; then kill "$gw_pid" 2>/dev/null || true; wait "$gw_pid" 2>/dev/null || true; fi
  rm -rf "$workdir"
}
trap cleanup EXIT

mkdir -p "$workdir/data"
versitygw --access "$access" --secret "$secret" --port "127.0.0.1:$port" \
  posix "$workdir/data" >"$workdir/gw.log" 2>&1 &
gw_pid=$!

s3() { curl -sf --aws-sigv4 "aws:amz:us-east-1:s3" --user "$access:$secret" "$@"; }
for _ in $(seq 1 50); do
  s3 -o /dev/null "$endpoint/" && break
  sleep 0.2
done
s3 -o /dev/null "$endpoint/" || { echo "S3 server did not start:"; cat "$workdir/gw.log"; exit 1; }
s3 -X PUT "$endpoint/radiant-test" >/dev/null

export RADIANT_TEST_S3_BUCKET=radiant-test RADIANT_TEST_S3_ENDPOINT="$endpoint"
export AWS_ACCESS_KEY_ID=$access AWS_SECRET_ACCESS_KEY=$secret
cargo test --locked "$@" s3
