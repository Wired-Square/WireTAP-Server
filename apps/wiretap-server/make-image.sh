#!/bin/sh
# Builds the wiretap-server app image, the capture daemon and its web UI, as a
# `docker load` tarball, one per architecture:
# target/apps/wiretap-server-<version>-<arch>.tar.
#
#     apps/wiretap-server/make-image.sh            arm64 and amd64
#     apps/wiretap-server/make-image.sh amd64      one of them
#
# The label is manifest.json with the daemon's version stamped in, so the label,
# the tag and the OCI version label cannot disagree.
set -eu

die() {
	echo "make-image: $1" >&2
	exit 1
}

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
cd -- "$HERE/../.."

for tool in cargo cargo-zigbuild docker jq npm; do
	command -v "$tool" >/dev/null || die "$tool is not on PATH"
done
docker buildx version >/dev/null 2>&1 || die "docker buildx is not installed"
[ -d "$HERE/frontend/node_modules" ] || die "run npm ci in $HERE/frontend first"

PKGID=$(cargo pkgid -p wiretap-server)
VERSION=${PKGID##*[#@]}
case $VERSION in *[+~]*) die "$VERSION is not a valid image tag: drop the + or ~" ;; esac
MANIFEST=$(jq -c --arg version "$VERSION" '{schema, name, version: $version} + .' "$HERE/manifest.json")
NAME=$(echo "$MANIFEST" | jq -r .name)
BUILD_ID=$(packaging/build-id.sh)
OUT=target/apps
LLD="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/rust-lld"

[ "$#" -gt 0 ] || set -- arm64 amd64

CONTEXT=$(mktemp -d)
trap 'rm -rf "$CONTEXT"' EXIT
trap 'exit 1' INT TERM HUP
cp "$HERE/entrypoint.sh" "$HERE/wiretap-server.toml" "$CONTEXT/"
npm --prefix "$HERE/frontend" run build >/dev/null
cp -R "$HERE/frontend/dist" "$CONTEXT/ui"

# The UI is linked as its package is, through the chassis's build-binary.sh,
# which also refuses a binary with no build id or an unpatched Cortex-A53 site.
build_ui() (
	cd "$HERE"
	unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
	export CARGO_BUILD_TARGET="$1"
	export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER:-$LLD}"
	export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-$LLD}"
	export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-arg=--fix-cortex-a53-843419"
	BINARY=wiretap-server-ui
	TARGET_DIR=target
	. scaffold/packaging/build-binary.sh
	binary_arch
	build_binary
	cp "$BIN" "$CONTEXT/"
)

# A builder of this script's own: the docker driver cannot write a `docker load`
# tarball on the classic image store.
BUILDER=appliance-image
docker buildx inspect "$BUILDER" >/dev/null 2>&1 ||
	docker buildx create --name "$BUILDER" --driver docker-container >/dev/null 2>&1 ||
	docker buildx inspect "$BUILDER" >/dev/null

mkdir -p "$OUT"
for arch; do
	case $arch in
	arm64) target=aarch64-unknown-linux-musl ;;
	amd64) target=x86_64-unknown-linux-musl ;;
	*) die "unknown architecture $arch: arm64 or amd64" ;;
	esac
	echo "$MANIFEST" | jq -e --arg arch "$arch" '.arches | index($arch)' >/dev/null ||
		die "manifest.json does not list $arch among its arches"

	cargo zigbuild --release --locked -p wiretap-server --target "$target"
	cp "target/$target/release/wiretap-server" "$CONTEXT/"
	build_ui "$target"

	image="$OUT/$NAME-$VERSION-$arch.tar"
	docker buildx build --builder "$BUILDER" --platform "linux/$arch" --file "$HERE/Dockerfile" \
		--build-arg "VERSION=$VERSION" --build-arg "MANIFEST=$MANIFEST" \
		--build-arg "WIRETAP_BUILD_ID=$BUILD_ID" \
		--tag "ghcr.io/wired-square/$NAME:$VERSION" \
		--output "type=docker,dest=$image.part" "$CONTEXT"
	mv "$image.part" "$image"
	echo "built $image"
done
