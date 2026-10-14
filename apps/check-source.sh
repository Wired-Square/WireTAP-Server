#!/usr/bin/env bash
# Proves make-source.sh's archive is one ApplianceOS accepts, by ApplianceOS's
# own code: it signs the archive with throwaway keys, copies ApplianceOS's
# console out of a checkout, and runs check-source.rs as one of its tests.
#
#     apps/check-source.sh <ApplianceOS checkout> [<unsigned.tar>]
#
# Without an archive it packs the apps' tarballs at the crate's version.
# APPLIANCEOS_REV picks the commit (default HEAD). Everything it writes is
# under target/apps/source-check.
set -euo pipefail

[[ $# == 1 || $# == 2 ]] || {
	sed -n 's/^#     apps/usage: apps/p' "$0" >&2
	exit 2
}
HERE=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=apps/make-source.sh
. "$HERE/make-source.sh"
applianceos=$1
work=$ROOT/target/apps/source-check
case=$work/case
rm -rf "$work/keys" "$work/other" "$case" "$work/applianceos"
mkdir -p "$case" "$work/applianceos"

unsigned=${2:-$case/unsigned.tar}
[[ $# == 2 ]] || pack "$unsigned"
test_keys "$work/keys"
test_keys "$work/other"
issue "$work/keys" server-auth serverAuth
(sign "$unsigned" "$case/signed.tar" "$work/keys/leaf.pem" "$work/keys/leaf.key")
tar -xf "$case/signed.tar" -C "$case"
cp "$work/keys/root.pem" "$case/root.pem"
cp "$work/other/root.pem" "$case/other-root.pem"
"$openssl" cms -sign -binary -md sha256 -nosmimecap -in "$case/index.json" -signer "$work/keys/server-auth.pem" \
	-inkey "$work/keys/server-auth.key" -outform DER -out "$case/server-auth.sig"
jq -c '.apps[0].manifest.version += "-tampered"' "$case/index.json" >"$case/tampered.json"

git -C "$applianceos" archive "${APPLIANCEOS_REV:-HEAD}" console | tar -x -C "$work/applianceos"
printf '\n#[path = "%s"]\nmod wiretap_source;\n' "$HERE/check-source.rs" \
	>>"$work/applianceos/console/crates/proxy/src/install/tests.rs"
WIRETAP_SOURCE_CASE=$case CARGO_TARGET_DIR=$work/target \
	cargo test --manifest-path "$work/applianceos/console/Cargo.toml" -p applianceos-proxy --lib wiretap_source
