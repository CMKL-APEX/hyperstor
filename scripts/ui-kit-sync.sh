#!/usr/bin/env bash
# The copy of glidex's @glidex/ui-kit in crates/hyperstor-mgmt/ui-kit
# (01-implementation-notes.md I140).
#
#   scripts/ui-kit-sync.sh REV      replace it with glidex's at commit REV
#   scripts/ui-kit-sync.sh --check  fail if it differs from its UPSTREAM rev
#
# The kit is TypeScript source glidex doesn't publish; hyperstor's UI
# builds it from this copy, as cargo builds glidex-auth from a pinned rev.
set -euo pipefail
cd "$(dirname "$0")/.."
kit=crates/hyperstor-mgmt/ui-kit
repo=${GLIDEX_REPO:-https://github.com/CMKL-APEX/glidex}
check=false
case "${1:-}" in
  --check) check=true; rev=$(awk '$1 == "rev" {print $2}' "$kit/UPSTREAM") ;;
  "") echo "usage: $0 REV | --check" >&2; exit 2 ;;
  *) rev=$1 ;;
esac
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git -C "$tmp" init -q
git -C "$tmp" fetch -q --depth 1 "$repo" "$rev"
mkdir "$tmp/out"
git -C "$tmp" archive FETCH_HEAD crates/glidex-ui/kit | tar -x -C "$tmp/out"
up="$tmp/out/crates/glidex-ui/kit"
if $check; then
  # Everything but UPSTREAM is glidex's, byte for byte.
  if ! diff -r --exclude=UPSTREAM --exclude=node_modules "$up" "$kit"; then
    echo "$kit differs from glidex $rev: change it in glidex, then $0 <commit>" >&2
    exit 1
  fi
  echo "ui-kit: matches glidex $rev"
  exit 0
fi
find "$kit" -mindepth 1 -maxdepth 1 ! -name UPSTREAM ! -name node_modules -exec rm -rf {} +
cp -r "$up/." "$kit/"
sed -i "s/^rev .*/rev $(git -C "$tmp" rev-parse FETCH_HEAD)/" "$kit/UPSTREAM"
echo "ui-kit: now glidex $(git -C "$tmp" rev-parse --short FETCH_HEAD)"
