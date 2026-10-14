#!/bin/sh
# Builds the wiretap-gateway app image as a `docker load` tarball, one per
# architecture: target/apps/wiretap-gateway-<version>-<arch>.tar.
#
#     apps/wiretap-gateway/make-image.sh            arm64 and amd64
#     apps/wiretap-gateway/make-image.sh amd64      one of them
set -eu

PACKAGE=wiretap-backend

stage() {
	cp "$HERE/entrypoint.sh" "$CONTEXT/"
	tar -C crates/wiretap-backend --exclude node_modules --exclude dist -cf - admin-ui | tar -C "$CONTEXT" -xf -
}

stage_arch() {
	cargo zigbuild --release --locked -p "$PACKAGE" --target "$1"
	cp "target/$1/release/$PACKAGE" "$CONTEXT/"
}

. "$(dirname -- "$0")/../image.sh"
