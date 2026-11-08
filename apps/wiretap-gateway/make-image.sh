#!/bin/sh
# Builds the wiretap-gateway app image, the gateway, the capture daemon and its
# web UI, as a `docker load` tarball, one per architecture:
# target/apps/wiretap-gateway-<version>-<arch>.tar.
#
#     apps/wiretap-gateway/make-image.sh            arm64 and amd64
#     apps/wiretap-gateway/make-image.sh amd64      one of them
set -eu

PACKAGE=wiretap-backend

stage() {
	cp "$HERE/entrypoint.sh" "$CONTEXT/"
	cat apps/wiretap-server/wiretap-server.toml "$HERE/forward.toml" >"$CONTEXT/wiretap-server.toml"
	tar -C crates/wiretap-backend --exclude node_modules --exclude dist -cf - admin-ui | tar -C "$CONTEXT" -xf -
	stage_server_ui
}

stage_arch() {
	cargo zigbuild --release --locked -p "$PACKAGE" -p wiretap-server --target "$1"
	cp "target/$1/release/$PACKAGE" "target/$1/release/wiretap-server" "$CONTEXT/"
	stage_server_ui_binary "$1"
}

. "$(dirname -- "$0")/../image.sh"
