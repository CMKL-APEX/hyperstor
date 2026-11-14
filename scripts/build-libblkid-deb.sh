#!/bin/sh
# Builds libblkid1 with Hyperstor's prober (packaging/libblkid/hyperstor.c)
# for one Ubuntu release, in a container, and checks it against device
# images before keeping it (D332).
#
#   scripts/build-libblkid-deb.sh RELEASE OUTDIR    e.g. 24.04 dist/
#
# The package is the release's own util-linux at its current version, with
# "+hyperstor1" appended: apt replaces it with Ubuntu's next util-linux
# update, and `hyperstor-install check` then warns until the matching build
# is installed again (D334). Needs podman or docker, and cargo for the
# images.
set -eu

release=${1:?usage: build-libblkid-deb.sh RELEASE OUTDIR}
out=${2:?usage: build-libblkid-deb.sh RELEASE OUTDIR}
here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)

if command -v podman >/dev/null 2>&1; then run=podman; else run=docker; fi

imgs=$(mktemp -d)
trap 'rm -rf "$imgs"' EXIT
cargo run -q --release -p hyperstor-engine --example blkid_images -- "$imgs"

"$run" run --rm \
	-v "$here/packaging/libblkid:/src:ro" \
	-v "$imgs:/img:ro" \
	-v "$out:/out" \
	"docker.io/library/ubuntu:$release" sh -euc '
export DEBIAN_FRONTEND=noninteractive
sed -i "s/^Types: deb\$/Types: deb deb-src/" /etc/apt/sources.list.d/ubuntu.sources
apt-get update -qq
apt-get install -y -qq --no-install-recommends dpkg-dev devscripts ca-certificates python3 >/dev/null
apt-get build-dep -y -qq util-linux >/dev/null
mkdir /build && cd /build
apt-get source -qq util-linux 2>/dev/null
cd util-linux-*/
python3 /src/register.py .
export DEBEMAIL="hyperstor-build@invalid" DEBFULLNAME="Hyperstor build"
dch --local +hyperstor "libblkid: recognise Hyperstor devices." >/dev/null 2>&1
version=$(dpkg-parsechangelog -S Version)
DEB_BUILD_OPTIONS="nocheck nodoc parallel=2" dpkg-buildpackage -b -uc -us >/build/log 2>&1 \
	|| { tail -40 /build/log; exit 1; }
deb=$(ls /build/libblkid1_"$version"_*.deb)
dpkg -i "$deb" >/dev/null
# The checks: both formats recognised with their ids, a damaged
# superblock not, and wipefs naming the signature it would erase.
check() { got=$(blkid -p -o export "/img/$1" | grep -E "^(TYPE|VERSION|UUID|LABEL)=" | sort | tr "\n" " "); [ "$got" = "$2" ] || { echo "$1: got \"$got\", want \"$2\""; exit 1; }; }
check v2.img "LABEL=device-1002 TYPE=hyperstor UUID=01234567-89ab-cdef-0011-223344556677 VERSION=2 "
check v1.img "LABEL=device-1002 TYPE=hyperstor UUID=01234567-89ab-cdef-0011-223344556677 VERSION=1 "
if blkid -p "/img/bad.img" >/dev/null 2>&1; then echo "bad.img: recognised"; exit 1; fi
wipefs -n /img/v2.img | grep -q hyperstor || { echo "wipefs: no hyperstor signature"; exit 1; }
blkid -k | grep -qx hyperstor
cp "$deb" /out/
echo "built $(basename "$deb")"
'
