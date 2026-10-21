#!/usr/bin/env bash
# Runs hyperstor's benchmark suites and saves their raw output, with the host
# they ran on, under target/decision-data/<timestamp>/.
#
#   scripts/collect-decision-data.sh [suite ...]
#
# Suites: crc placement mem raw engine index decisions redb cluster failover adminload
# replay logsize
# (default: crc placement decisions redb). Scratch files go to
# $HS_SCRATCH (default: ~/hs-scratch, on the disk being measured; never a
# tmpfs, which can't do O_DIRECT and measures memory instead of the device),
# and are removed afterwards. HS_BENCH_SECS sets seconds per test (default 3).
set -euo pipefail
cd "$(dirname "$0")/.."
suites=("$@")
[ ${#suites[@]} -eq 0 ] && suites=(crc placement decisions redb)
scratch="${HS_SCRATCH:-$HOME/hs-scratch}"
out="target/decision-data/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$out" "$scratch"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
cargo build --release -j2 -p hyperstor-bench >/dev/null
bench="$CARGO_TARGET_DIR/release/hyperstor-bench"
{
  echo "date: $(date -Iseconds)"
  echo "commit: $(git rev-parse --short HEAD) ($(git status --porcelain | wc -l) uncommitted files)"
  echo "kernel: $(uname -r)"
  lscpu | grep -E "Model name|^CPU\(s\)|Socket"
  echo "memory: $(free -g | awk '/Mem:/ {print $2 " GiB total, " $7 " GiB available"}')"
  echo "scratch: $scratch on $(df -T "$scratch" | awk 'NR==2 {print $1 " (" $2 ")"}')"
  for d in /sys/block/nvme*n*; do
    [ -e "$d" ] || continue
    echo "$(basename "$d"): $(cat "$d/device/model" 2>/dev/null | xargs), write cache $(cat "$d/queue/write_cache"), fua $(cat "$d/queue/fua")"
  done
} > "$out/host.txt"
for s in "${suites[@]}"; do
  echo "== $s" >&2
  case "$s" in
    crc|placement|mem|failover|replay|logsize|adminload) "$bench" "$s" ;;
    raw|engine) "$bench" "$s" "$scratch/dev.img" 2 ;;
    index) "$bench" index "$scratch/index.img" ;;
    decisions|redb) "$bench" "$s" "$scratch" ;;
    cluster) "$bench" cluster mem 1 ;;
    *) echo "unknown suite $s" >&2; exit 2 ;;
  esac > "$out/$s.md" 2>&1
done
rm -rf "$scratch"
echo "$out"
