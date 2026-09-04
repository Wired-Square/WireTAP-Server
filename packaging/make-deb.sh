#!/usr/bin/env bash
#
# Build the wiretap-server .deb, from a clean tree to a package.
#
#   packaging/make-deb.sh                 arm64 (the appliance), build and package
#   packaging/make-deb.sh --arch amd64    x86-64 instead
#   packaging/make-deb.sh --arch all      both, from one source tree
#   packaging/make-deb.sh --skip-build    package a binary already in target/
#   packaging/make-deb.sh --version 0.1.1 override the version from Cargo.toml
#
# Output: target/deb/wiretap-server_<version>_<arch>.deb
#
# Both architectures are static musl builds, so one package installs on any
# distribution with systemd - the Pi appliance on arm64, a VM or an LXC
# container on amd64.
#
# Why not dpkg-buildpackage: nothing here is compiled by the packaging. The
# binary is cross-compiled by cargo-zigbuild, which is the toolchain that works
# on macOS, and dpkg-deb assembles the result. So this runs on the machine the
# binary is already built on rather than requiring a Debian build host.
#
# debian/ stays the source of truth for the control metadata and the maintainer
# scripts; this reads them rather than restating them. The one field it does not
# read is Architecture, which is `any` there and has to be concrete here.

set -euo pipefail

PKG=wiretap-server

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${HERE}/.." && pwd)"
DEBIAN="${ROOT}/debian"

skip_build=0
version=""
arches=(arm64)
while [ $# -gt 0 ]; do
	case "$1" in
		--skip-build) skip_build=1 ;;
		--version) version="${2:?--version needs a value}"; shift ;;
		--arch)
			case "${2:?--arch needs a value}" in
				arm64) arches=(arm64) ;;
				amd64) arches=(amd64) ;;
				all)   arches=(arm64 amd64) ;;
				*) echo "unknown architecture: $2 (want arm64, amd64 or all)" >&2; exit 2 ;;
			esac
			shift ;;
		-h|--help) sed -n '2,25p' "$0"; exit 0 ;;
		*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
	shift
done

die() { echo "make-deb: $*" >&2; exit 1; }
say() { echo "==> $*"; }
field() {  # field <stanza> <key>
	printf '%s\n' "$1" | awk -v k="$2" '$0 ~ "^"k": " { sub("^"k": ", ""); print; exit }'
}

command -v dpkg-deb >/dev/null || die "dpkg-deb not found (macOS: brew install dpkg)"

if [ -z "${version}" ]; then
	version="$(awk '/^\[workspace\.package\]/{f=1} f && /^version[[:space:]]*=/{gsub(/[",]/,"",$3); print $3; exit}' \
		"${ROOT}/Cargo.toml")"
fi
[ -n "${version}" ] || die "could not read the version from Cargo.toml"
say "version ${version}, architectures: ${arches[*]}"

# --- what the package promises about itself -------------------------------
# Checked once, before any compiling: these are properties of the tree rather
# than of an architecture, and each costs far more to discover after the fact.
UNIT="${ROOT}/packaging/${PKG}.service"
CONFIG="${ROOT}/packaging/${PKG}.toml"
EXAMPLE="${ROOT}/packaging/examples/can-interface.service"
PROTOCOL="${ROOT}/docs/ingest-protocol.md"

for f in "${UNIT}" "${CONFIG}" "${EXAMPLE}" "${PROTOCOL}"; do
	[ -f "${f}" ] || die "missing ${f#"${ROOT}"/}"
done

if ! grep -qE '^ExecStart=/usr/bin/wiretap-server( |$)' "${UNIT}"; then
	die "packaging/${PKG}.service does not ExecStart /usr/bin/wiretap-server.
     The package installs to /usr/bin; a unit naming /usr/local/bin would
     produce a box whose daemon points at a path dpkg does not manage."
fi

# The unit passes -C because the daemon has NO default config path - it reads a
# file only when told to. Drop the flag and the package silently installs a
# configuration file that nothing ever opens, and the daemon runs on built-in
# defaults instead.
if ! grep -qE '^ExecStart=.* -C /etc/wiretap-server/wiretap-server.toml( |$)' "${UNIT}"; then
	die "packaging/${PKG}.service does not pass -C /etc/wiretap-server/wiretap-server.toml.
     The daemon has no default config path, so without that flag the file the
     postinst writes is never read."
fi

# The two address families that are easy to omit and fail at runtime rather
# than here: AF_CAN is the capture itself, and AF_NETLINK is the bitrate query
# that replaced pyroute2. Omitting either gives EAFNOSUPPORT from inside the
# sandbox, which reads like broken hardware.
for family in AF_CAN AF_NETLINK; do
	grep -qE "^RestrictAddressFamilies=.*\\b${family}\\b" "${UNIT}" \
		|| die "packaging/${PKG}.service restricts address families without ${family}."
done

# --- the install layout, declared once ------------------------------------
# The unit's copy of these paths is asserted above. The postinst has its own,
# and the daemon has a third in Rust - and nothing at runtime notices when they
# stop agreeing. It just quietly does the wrong thing: a config written where
# the unit does not look, or an outage's frames moved somewhere the daemon will
# not open. So they are compared here, where a mismatch is a failed build.
CONF_DIR=/etc/wiretap-server
REF_PATH=/usr/share/${PKG}/${PKG}.toml
DOC_DIR=/usr/share/doc/${PKG}
STATE_DIR=/var/lib/${PKG}
CACHE_FILE=cache.db
LEGACY_CACHE_FILE=.wiretap-server-cache.db

postinst_var() {  # postinst_var <name> — the literal it is assigned
	sed -n "s/^$1=//p" "${DEBIAN}/postinst" | head -1 | tr -d '"'
}
same() {  # same <what> <wanted> <got>
	[ "$2" = "$3" ] || die "debian/postinst disagrees about $1:
     make-deb.sh says ${2}
     postinst says   ${3}"
}

same "the config directory" "${CONF_DIR}"  "$(postinst_var CONFIG_DIR)"
same "the packaged reference config" "${REF_PATH}" "$(postinst_var REFERENCE_TOML)"
same "the state directory" "${STATE_DIR}" "$(postinst_var STATE_DIR)"
same "the legacy cache filename" "${LEGACY_CACHE_FILE}" "$(postinst_var LEGACY_CACHE_FILE)"
same "the cache path" "\$STATE_DIR/${CACHE_FILE}" "$(postinst_var CACHE)"

# The cache filename is the daemon's, not the packaging's: the postinst moves an
# outage's frames to a path only `settings.rs` decides. Rename it there and the
# frames land somewhere nothing opens, silently, during an upgrade mid-outage.
SETTINGS="${ROOT}/crates/wiretap-server/src/settings.rs"
grep -qF "join(\"${CACHE_FILE}\")" "${SETTINGS}" \
	|| die "settings.rs no longer defaults the cache to ${CACHE_FILE};
     debian/postinst moves a legacy cache to that name."
grep -qF "\"${LEGACY_CACHE_FILE}\"" "${SETTINGS}" \
	|| die "settings.rs no longer knows ${LEGACY_CACHE_FILE};
     debian/postinst still migrates from it."

build_arch() {  # build_arch <arch>
	local arch="$1" target machine
	# One table rather than a function per column: an architecture is a rust
	# target and the ELF e_machine that proves the target took, and splitting
	# them means adding one edits two places that have to agree.
	# (183 = AArch64, 62 = x86-64.)
	case "${arch}" in
		arm64) target=aarch64-unknown-linux-musl; machine=183 ;;
		amd64) target=x86_64-unknown-linux-musl;  machine=62 ;;
		*) die "no target mapping for architecture ${arch}" ;;
	esac

	if [ "${skip_build}" -eq 0 ]; then
		command -v cargo-zigbuild >/dev/null || die "cargo-zigbuild not found (brew install cargo-zigbuild zig)"
		rustup target list --installed 2>/dev/null | grep -qx "${target}" \
			|| die "rust target ${target} not installed - rustup target add ${target}"

		# Per-crate, not as a workspace: cargo unifies features across a
		# workspace, so a workspace build would resolve shared crates with the
		# gateway's feature set and link a TLS stack and a Postgres client into
		# a daemon that speaks neither.
		say "[${arch}] cross-compiling ${PKG}"
		( cd "${ROOT}" && cargo zigbuild --target "${target}" --release -p "${PKG}" )
	fi

	local bin="${ROOT}/target/${target}/release/${PKG}"
	[ -f "${bin}" ] || die "[${arch}] missing ${PKG} - run without --skip-build"

	# --- sanity checks on what we are about to ship -----------------------
	# The ELF header read directly, so this depends on neither `file` being
	# installed nor on how it words itself. e_machine is at offset 18, LE.
	local got
	got="$(od -An -tu1 -j18 -N1 "${bin}" | tr -d ' ')"
	[ "${got}" = "${machine}" ] \
		|| die "[${arch}] ${PKG} has e_machine=${got}, wanted ${machine}.
     A binary from a previous --arch run is still sitting in target/."

	# Dynamically linked would mean the musl target silently did not take, and
	# the binary would not run on a host whose glibc does not match. A static
	# ELF has no PT_INTERP, so there is no interpreter string in it.
	if command -v strings >/dev/null 2>&1; then
		if strings -a "${bin}" 2>/dev/null | grep -qE '^/lib.*ld-linux'; then
			die "[${arch}] ${PKG} looks dynamically linked - the musl target did not take"
		fi
	fi

	# A size floor, because "it compiled" is not proof a dependency linked. This
	# binary measured 3.41 MB once the disk cache was actually reachable from
	# it, and 1.6 MB before that - when SQLite was compiled, linked, and then
	# dead-code-eliminated because nothing called it. A build that comes out
	# small is a build missing the thing that carries a gateway outage.
	local bytes
	bytes="$(wc -c < "${bin}" | tr -d ' ')"
	if [ "${bytes}" -lt 2500000 ]; then
		die "[${arch}] ${PKG} is ${bytes} bytes, too small to be carrying SQLite.
     Something was dead-code-eliminated; the disk cache is the usual casualty."
	fi
	say "[${arch}] ${PKG} ${bytes} bytes"

	# --- stage ------------------------------------------------------------
	local stage out
	stage="${ROOT}/target/deb/${PKG}_${version}_${arch}"
	out="${ROOT}/target/deb/${PKG}_${version}_${arch}.deb"
	rm -rf "${stage}"
	mkdir -p "${stage}/DEBIAN" \
	         "${stage}/usr/bin" \
	         "${stage}/usr/lib/systemd/system" \
	         "${stage}$(dirname "${REF_PATH}")" \
	         "${stage}${DOC_DIR}/examples"

	install -m 0755 "${bin}"  "${stage}/usr/bin/${PKG}"
	install -m 0644 "${UNIT}" "${stage}/usr/lib/systemd/system/${PKG}.service"
	# The reference configuration, which the postinst copies into /etc when
	# there is nothing there. Kept on the box so an upgrade can be diffed
	# against what this version ships.
	install -m 0644 "${CONFIG}"   "${stage}${REF_PATH}"
	install -m 0644 "${PROTOCOL}" "${stage}${DOC_DIR}/ingest-protocol.md"
	# Documentation, not a unit: bringing a CAN interface up is the host's job
	# and the scope of what this package should own is still open.
	install -m 0644 "${EXAMPLE}"  "${stage}${DOC_DIR}/examples/can-interface.service"
	install -m 0644 "${DEBIAN}/copyright" "${stage}${DOC_DIR}/copyright"
	gzip -9nc "${DEBIAN}/changelog" > "${stage}${DOC_DIR}/changelog.Debian.gz"
	chmod 0644 "${stage}${DOC_DIR}/changelog.Debian.gz"

	local s
	for s in postinst prerm postrm; do
		install -m 0755 "${DEBIAN}/${s}" "${stage}/DEBIAN/${s}"
	done

	# Control: assembled from debian/control rather than restated here, so the
	# dependencies and the description have one home. Paragraph 1 is the source
	# stanza, paragraph 2 the binary one.
	local src_stanza bin_stanza installed_kb
	src_stanza="$(awk 'BEGIN{RS=""} NR==1' "${DEBIAN}/control")"
	bin_stanza="$(awk 'BEGIN{RS=""} NR==2' "${DEBIAN}/control")"
	installed_kb="$(du -sk "${stage}" | cut -f1)"

	{
		echo "Package: $(field "${bin_stanza}" Package)"
		echo "Version: ${version}"
		echo "Section: $(field "${src_stanza}" Section)"
		echo "Priority: $(field "${src_stanza}" Priority)"
		echo "Architecture: ${arch}"
		echo "Depends: $(field "${bin_stanza}" Depends)"
		echo "Suggests: $(field "${bin_stanza}" Suggests)"
		echo "Installed-Size: ${installed_kb}"
		echo "Maintainer: $(field "${src_stanza}" Maintainer)"
		echo "Homepage: $(field "${src_stanza}" Homepage)"
		# Description is a multi-line field and must come last in the stanza.
		printf '%s\n' "${bin_stanza}" | awk '/^Description: /{f=1} f'
	} > "${stage}/DEBIAN/control"

	# md5sums is optional; skip it rather than fail when md5sum is absent
	# (macOS ships `md5`, and coreutils may not be on PATH).
	if command -v md5sum >/dev/null 2>&1; then
		( cd "${stage}" && find . -type f ! -path './DEBIAN/*' -exec md5sum {} + \
			| sed 's| \./| |' > DEBIAN/md5sums )
		chmod 0644 "${stage}/DEBIAN/md5sums"
	else
		echo "make-deb: md5sum not found, omitting DEBIAN/md5sums" >&2
	fi

	say "[${arch}] assembling ${out}"
	dpkg-deb --root-owner-group --build "${stage}" "${out}" >/dev/null

	dpkg-deb --info "${out}" | sed 's/^/    /'
	echo
	dpkg-deb --contents "${out}" | sed 's/^/    /'

	if command -v lintian >/dev/null 2>&1; then
		echo
		say "[${arch}] lintian"
		lintian --no-tag-display-limit "${out}" || true
	fi
	echo
}

for a in "${arches[@]}"; do
	build_arch "${a}"
done

say "done"
