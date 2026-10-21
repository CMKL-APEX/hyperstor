#!/usr/bin/env bash
# Hyperstor's end-to-end suite (crates/hyperstor-test).
#
#   scripts/e2e.sh              build, then install and test clusters in
#                               scratch roots (no root needed; what CI runs
#                               as part of cargo test --workspace)
#   scripts/e2e.sh --installed  also check the cluster really installed on
#                               this host (sudo hyperstor-install …), with
#                               the test binary run through sudo
#
# HS_E2E_KEEP=1 keeps scratch directories; CARGO_TARGET_DIR is honoured.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
jobs="${HS_JOBS:-2}"
cargo build --release --workspace -j"$jobs"
cargo test --release -j"$jobs" -p hyperstor-test
if [ "${1:-}" = "--installed" ]; then
  bin=$(cargo test --release -j"$jobs" -p hyperstor-test --test installed --no-run 2>&1 \
        | grep -o "$CARGO_TARGET_DIR/release/deps/installed-[0-9a-f]*" | head -1)
  # The binary under sudo, not cargo: cargo as root leaves root-owned files.
  sudo "$bin" --ignored
fi
