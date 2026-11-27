#!/usr/bin/env bash
# Runs the Cask tests against a throwaway 3-node Cask cluster. Extra arguments are passed to
# `cargo test`.
#
#   nix develop --command scripts/test-cask.sh
set -euo pipefail
exec "$(dirname "$0")/cask-cluster.sh" -n 3 -- cargo test --locked "$@" cask
