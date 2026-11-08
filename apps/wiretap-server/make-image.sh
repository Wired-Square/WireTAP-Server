#!/bin/sh
# Builds the wiretap-server app image, the capture daemon and its web UI, as a
# `docker load` tarball, one per architecture:
# target/apps/wiretap-server-<version>-<arch>.tar.
#
#     apps/wiretap-server/make-image.sh            arm64 and amd64
#     apps/wiretap-server/make-image.sh amd64      one of them
set -eu

PACKAGE=wiretap-server

stage() {
	cp "$HERE/entrypoint.sh" "$HERE/wiretap-server.toml" "$CONTEXT/"
	stage_server_ui
}

stage_arch() {
	cargo zigbuild --release --locked -p "$PACKAGE" --target "$1"
	cp "target/$1/release/$PACKAGE" "$CONTEXT/"
	stage_server_ui_binary "$1"
}

. "$(dirname -- "$0")/../image.sh"
