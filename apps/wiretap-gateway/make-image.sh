#!/bin/sh
# Builds the wiretap-gateway app image as a `docker load` tarball, one per
# architecture: target/apps/wiretap-gateway-<version>-<arch>.tar.
#
#     apps/wiretap-gateway/make-image.sh            arm64 and amd64
#     apps/wiretap-gateway/make-image.sh amd64      one of them
#
# The label is manifest.json with the crate's version stamped in, so the label,
# the tag and the OCI version label cannot disagree.
set -eu

die() {
	echo "make-image: $1" >&2
	exit 1
}

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
cd -- "$HERE/../.."

for tool in cargo cargo-zigbuild docker jq; do
	command -v "$tool" >/dev/null || die "$tool is not on PATH"
done
docker buildx version >/dev/null 2>&1 || die "docker buildx is not installed"

PKGID=$(cargo pkgid -p wiretap-backend)
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
cp "$HERE/entrypoint.sh" "$CONTEXT/"
tar -C crates/wiretap-backend --exclude node_modules --exclude dist -cf - admin-ui | tar -C "$CONTEXT" -xf -

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

	cargo zigbuild --release --locked -p wiretap-backend --target "$target"
	cp "target/$target/release/wiretap-backend" "$CONTEXT/"

	image="$OUT/$NAME-$VERSION-$arch.tar"
	docker buildx build --builder "$BUILDER" --platform "linux/$arch" --file "$HERE/Dockerfile" \
		--build-arg "VERSION=$VERSION" --build-arg "MANIFEST=$MANIFEST" \
		--build-arg "WIRETAP_BUILD_ID=$BUILD_ID" \
		--tag "ghcr.io/wired-square/$NAME:$VERSION" \
		--output "type=docker,dest=$image.part" "$CONTEXT"
	mv "$image.part" "$image"
	echo "built $image"
done
