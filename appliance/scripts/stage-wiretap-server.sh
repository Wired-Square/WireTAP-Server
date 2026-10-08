#!/bin/sh
# Copies wiretap-server's arm64 package, from the root workspace's
# packaging/make-deb.sh, to where appliance.toml's extra_debs finds it. Run it
# before scaffold/image/build-remote.sh.
set -eu

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(dirname -- "$(dirname -- "$HERE")")
DEST="$HERE/../target/debian/extra"

die() { echo "stage-wiretap-server: $*" >&2; exit 1; }

rm -f "$DEST"/wiretap-server_*_arm64.deb
set -- "$ROOT"/target/deb/wiretap-server_*_arm64.deb
[ -f "$1" ] || die "no target/deb/wiretap-server_*_arm64.deb - run packaging/make-deb.sh --arch arm64 from the repository root first"
[ "$#" -eq 1 ] || die "$# packages in target/deb, and the image takes one - delete the ones it should not: $*"

mkdir -p "$DEST"
cp "$1" "$DEST/"
echo "staged ${1##*/} for the image"
