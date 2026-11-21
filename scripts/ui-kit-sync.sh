#!/usr/bin/env bash
# The copy of @glidex/ui-kit in crates/hyperstor-mgmt/ui-kit: glidex-commons'
# ui-kit/, unchanged, at the release tag the Rust crates pin
# (D356; 01-implementation-notes.md I140).
#
#   scripts/ui-kit-sync.sh TAG      replace it with glidex-commons' at TAG
#   scripts/ui-kit-sync.sh --check  fail if it differs from its UPSTREAM tag
#
# bun can't install a package from a repository's subdirectory, so the UI
# builds the kit from this copy, as cargo builds the crates from a tag.
set -euo pipefail
cd "$(dirname "$0")/.."
kit=crates/hyperstor-mgmt/ui-kit
repo=${GLIDEX_COMMONS_REPO:-https://github.com/CMKL-APEX/glidex-commons}
check=false
case "${1:-}" in
  --check)
    check=true
    tag=$(awk '$1 == "tag" {print $2}' "$kit/UPSTREAM")
    rev=$(awk '$1 == "rev" {print $2}' "$kit/UPSTREAM")
    ;;
  "" | -*) echo "usage: $0 TAG | --check" >&2; exit 2 ;;
  *) tag=$1 ;;
esac
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git -C "$tmp" init -q
git -C "$tmp" fetch -q --depth 1 "$repo" "refs/tags/$tag"
got=$(git -C "$tmp" rev-parse 'FETCH_HEAD^{commit}')
mkdir "$tmp/out"
git -C "$tmp" archive FETCH_HEAD ui-kit | tar -x -C "$tmp/out"
up="$tmp/out/ui-kit"
if $check; then
  if [ "$got" != "$rev" ]; then
    echo "glidex-commons $tag is $got now, not $rev as $kit/UPSTREAM says" >&2
    exit 1
  fi
  # Everything but UPSTREAM is glidex-commons', byte for byte.
  if ! diff -r --exclude=UPSTREAM --exclude=node_modules "$up" "$kit"; then
    echo "$kit differs from glidex-commons $tag: change it there, then $0 <tag>" >&2
    exit 1
  fi
  echo "ui-kit: matches glidex-commons $tag"
  exit 0
fi
find "$kit" -mindepth 1 -maxdepth 1 ! -name UPSTREAM ! -name node_modules -exec rm -rf {} +
cp -r "$up/." "$kit/"
cat > "$kit/UPSTREAM" <<EOT
# A copy of glidex-commons' ui-kit/, unchanged, at this tag: the one the
# Rust crates from glidex-commons pin (Cargo.toml). Change it there, then
# scripts/ui-kit-sync.sh <tag>. CI checks it: scripts/ui-kit-sync.sh --check
tag $tag
rev $got
EOT
echo "ui-kit: now glidex-commons $tag ($got)"
