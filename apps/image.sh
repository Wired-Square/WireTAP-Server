# shellcheck shell=sh
# Sourced by apps/<name>/make-image.sh, once it has set PACKAGE and defined
# stage (the shared build context) and stage_arch (one architecture's
# binaries, given its Rust target), which may call the stage_server_ui
# functions below. Writes a `docker load` tarball per architecture:
# target/apps/<name>-<version>-<arch>.tar.
#
# The label is manifest.json with PACKAGE's version stamped in, so the label,
# the tag and the OCI version label cannot disagree.

die() {
	echo "make-image: $1" >&2
	exit 1
}

# The server app's browser half, built into the context's ui/.
stage_server_ui() {
	ui=apps/wiretap-server/frontend
	[ -d "$ui/node_modules" ] || die "run npm ci in $ui first"
	npm --prefix "$ui" run build >/dev/null
	cp -R "$ui/dist" "$CONTEXT/ui"
}

# wiretap-server-ui for the Rust target $1, linked as its package is, through
# the rendered build-binary.sh, which also refuses a binary with no build id or
# an unpatched Cortex-A53 site.
stage_server_ui_binary() {
	(
		cd apps/wiretap-server || exit
		unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
		lld="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/rust-lld"
		export CARGO_BUILD_TARGET="$1"
		export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER:-$lld}"
		export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-$lld}"
		export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-arg=--fix-cortex-a53-843419"
		BINARY=wiretap-server-ui
		TARGET_DIR=target
		. scaffold/packaging/build-binary.sh
		binary_arch
		build_binary
		cp "$BIN" "$CONTEXT/"
	)
}

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
cd -- "$HERE/../.." || die "cannot enter the repository root"
# shellcheck source=scripts/env.sh
. scripts/env.sh
load_env .

for tool in cargo cargo-zigbuild docker jq; do
	command -v "$tool" >/dev/null || die "$tool is not on PATH"
done
docker buildx version >/dev/null 2>&1 || die "docker buildx is not installed"

PKGID=$(cargo pkgid -p "$PACKAGE")
VERSION=${PKGID##*[#@]}
case $VERSION in *[+~]*) die "$VERSION is not a valid image tag: drop the + or ~" ;; esac
MANIFEST=$(jq -c --arg version "$VERSION" '{schema, name, version: $version} + .' "$HERE/manifest.json")
NAME=$(echo "$MANIFEST" | jq -r .name)
BUILD_ID=$(packaging/build-id.sh)
OUT=target/apps

[ "$#" -gt 0 ] || set -- arm64 amd64

CONTEXT=$(mktemp -d)
trap 'rm -rf "$CONTEXT"' EXIT
trap 'exit 1' INT TERM HUP
stage

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

	stage_arch "$target"

	image="$OUT/$NAME-$VERSION-$arch.tar"
	docker buildx build --builder "$BUILDER" --platform "linux/$arch" --file "$HERE/Dockerfile" \
		--build-arg "VERSION=$VERSION" --build-arg "MANIFEST=$MANIFEST" \
		--build-arg "WSLIB_BUILD_ID=$BUILD_ID" \
		--tag "ghcr.io/wired-square/$NAME:$VERSION" \
		--output "type=docker,dest=$image.part" "$CONTEXT"
	mv "$image.part" "$image"
	echo "built $image"
done
