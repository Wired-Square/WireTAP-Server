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
	[ -d "$HERE/frontend/node_modules" ] || die "run npm ci in $HERE/frontend first"
	cp "$HERE/entrypoint.sh" "$HERE/wiretap-server.toml" "$CONTEXT/"
	npm --prefix "$HERE/frontend" run build >/dev/null
	cp -R "$HERE/frontend/dist" "$CONTEXT/ui"
}

stage_arch() {
	cargo zigbuild --release --locked -p "$PACKAGE" --target "$1"
	cp "target/$1/release/$PACKAGE" "$CONTEXT/"
	# The UI is linked as its package is, through the rendered build-binary.sh,
	# which also refuses a binary with no build id or an unpatched Cortex-A53 site.
	(
		cd "$HERE"
		unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
		lld="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/rust-lld"
		export CARGO_BUILD_TARGET="$1"
		export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER:-$lld}"
		export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-$lld}"
		export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-arg=--fix-cortex-a53-843419"
		BINARY=wiretap-server-ui
		PACKAGE=$BINARY
		TARGET_DIR=target
		. scaffold/packaging/build-binary.sh
		binary_arch
		build_binary
		cp "$BIN" "$CONTEXT/"
	)
}

. "$(dirname -- "$0")/../image.sh"
