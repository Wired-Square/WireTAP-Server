#!/usr/bin/env bash
# The apps' ApplianceOS source: an archive of index.json, its detached CMS
# signature index.json.sig and the images' `docker load` tarballs at
# images/sha256-<hex>.tar, as the console's Apps › Install takes one. The format
# is ApplianceOS's scripts/make-app-archive.sh's; this one packs and signs
# apart, so a release can build the archive and the source's key stays off it.
#
#     apps/make-source.sh pack <out.tar> [<image.tar>...]
#     apps/make-source.sh sign <unsigned.tar> <out.tar> <cert.pem> <key> [<chain.pem>]
#     apps/make-source.sh test-keys <dir>
#
# pack defaults to both apps' tarballs for both arches at the crate's version.
# SOURCE names the source (default wiretap), as the box holds its root.
set -euo pipefail

SOURCE=${SOURCE:-wiretap}
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

die() {
	echo "make-source: $1" >&2
	exit 1
}

usage() {
	sed -n 's/^#     apps/usage: apps/p' "$0" >&2
	exit 2
}

openssl3() {
	local candidate
	for candidate in "${OPENSSL:-openssl}" /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
		if "$candidate" version 2>/dev/null | grep -q '^OpenSSL 3'; then
			openssl=$candidate
			return
		fi
	done
	die "wants OpenSSL 3; set OPENSSL"
}

# One index entry: the label's manifest pinned to the tarball's OCI manifest
# digest, which is the image ID a containerd-store engine gives it on load.
entry() {
	local image=$1 index digest arch name config
	index=$(tar -xOf "$image" index.json)
	[[ $(jq '.manifests | length' <<<"$index") == 1 ]] || die "$image holds more than one image"
	digest=$(jq -r '.manifests[0].digest' <<<"$index")
	arch=$(jq -r '.manifests[0].platform.architecture' <<<"$index")
	name=$(jq -r '.manifests[0].annotations["io.containerd.image.name"]' <<<"$index")
	config=$(tar -xOf "$image" "blobs/sha256/${digest#sha256:}" | jq -r '.config.digest')
	config=$(tar -xOf "$image" "blobs/sha256/${config#sha256:}")
	jq -c --arg arch "$arch" --arg repository "${name%:*}" --arg digest "$digest" '
		.config.Labels as $labels
		| $labels["com.wiredsquare.appliance.manifest"] // error("the image carries no manifest label")
		| fromjson
		| if .version != $labels["org.opencontainers.image.version"] then error("its label and its OCI version disagree")
		  elif (.arches | index($arch)) == null then error("its manifest does not list \($arch)")
		  else {arch: $arch, manifest: (. + {image: {repository: $repository, digest: $digest}})} end' <<<"$config" ||
		die "$image is not an app image"
}

# A scratch directory beside <out>, so that <out> appears whole, by a rename, or not at all.
stage_beside() {
	staging=$(mktemp -d "$1.XXXXXX")
	trap 'rm -rf "$staging"' EXIT
}

pack() {
	local out=$1 version image digest
	shift
	if [[ $# == 0 ]]; then
		version=$(cargo pkgid --manifest-path "$ROOT/Cargo.toml" -p wiretap-server)
		version=${version##*[#@]}
		set -- "$ROOT"/target/apps/wiretap-{gateway,server}-"$version"-{arm64,amd64}.tar
	fi
	stage_beside "$out"
	mkdir "$staging/images"
	for image; do
		[[ -f $image ]] || die "no $image; build it with apps/<name>/make-image.sh"
		entry "$image" >>"$staging/entries"
		digest=$(tail -n 1 "$staging/entries" | jq -r '.manifest.image.digest')
		cp "$image" "$staging/images/${digest/:/-}.tar"
	done
	jq -s -c --arg source "$SOURCE" '
		if group_by([.manifest.name, .manifest.version, .arch]) | any(length > 1)
		then error("two images are the same app, version and arch") else {schema: 1, source: $source, apps: .} end' \
		"$staging/entries" >"$staging/index.json"
	COPYFILE_DISABLE=1 tar --format ustar -cf "$staging/archive.tar" -C "$staging" index.json images
	mv "$staging/archive.tar" "$out"
	echo "$out, unsigned: $(jq -r '[.apps[] | "\(.manifest.name) \(.manifest.version) \(.arch)"] | join(", ")' "$staging/index.json") from $SOURCE"
}

sign() {
	local unsigned=$1 out=$2 cert=$3 key=$4 chain=${5-}
	openssl3
	! tar -tf "$unsigned" | grep -x index.json.sig >/dev/null || die "$unsigned is signed already"
	stage_beside "$out"
	tar -xf "$unsigned" -C "$staging" index.json
	"$openssl" cms -sign -binary -md sha256 -nosmimecap -in "$staging/index.json" -signer "$cert" -inkey "$key" \
		${chain:+-certfile "$chain"} -outform DER -out "$staging/index.json.sig"
	cp "$unsigned" "$staging/archive.tar"
	COPYFILE_DISABLE=1 tar --format ustar -rf "$staging/archive.tar" -C "$staging" index.json.sig
	mv "$staging/archive.tar" "$out"
	echo "$out, signed by $("$openssl" x509 -in "$cert" -noout -subject)"
}

# A throwaway root and Code Signing leaf, for tests only.
test_keys() {
	local dir=$1
	openssl3
	mkdir -p "$dir"
	[[ ! -e $dir/root.key ]] || die "$dir already holds keys"
	(umask 077 && "$openssl" req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc -keyout "$dir/root.key" \
		-sha256 -days 30 -subj "/CN=Throwaway test source root" -addext "basicConstraints=critical,CA:TRUE" \
		-addext "keyUsage=critical,keyCertSign,cRLSign" -out "$dir/root.pem" 2>/dev/null)
	issue "$dir" leaf codeSigning
	echo "$dir: root.pem, leaf.pem and leaf.key; test only"
}

# <dir>/<name>.pem and .key, a leaf under <dir>'s root for the extended key usage <eku>.
issue() {
	local dir=$1 name=$2 eku=$3
	(
		umask 077
		"$openssl" req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc -keyout "$dir/$name.key" \
			-subj "/CN=Throwaway test source $name" -out "$dir/$name.csr" 2>/dev/null
		printf '%s\n' "basicConstraints=critical,CA:FALSE" "keyUsage=critical,digitalSignature" \
			"extendedKeyUsage=critical,$eku" >"$dir/$name.ext"
		"$openssl" x509 -req -in "$dir/$name.csr" -CA "$dir/root.pem" -CAkey "$dir/root.key" -CAcreateserial \
			-sha256 -days 30 -extfile "$dir/$name.ext" -out "$dir/$name.pem" 2>/dev/null
		rm "$dir/$name.csr" "$dir/$name.ext"
	)
}

[[ ${BASH_SOURCE[0]} == "$0" ]] || return 0
command -v jq >/dev/null || die "jq is not on PATH"
case ${1-} in
pack) [[ $# -ge 2 ]] || usage && pack "${@:2}" ;;
sign) [[ $# == 5 || $# == 6 ]] || usage && sign "${@:2}" ;;
test-keys) [[ $# == 2 ]] || usage && test_keys "$2" ;;
*) usage ;;
esac
