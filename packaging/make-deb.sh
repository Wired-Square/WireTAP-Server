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
		# The header above, however long it is: a hardcoded line span silently
		# truncates the day somebody adds a paragraph to it.
		-h|--help) awk 'NR>1 { if (!/^#/) exit; print }' "$0"; exit 0 ;;
		*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
	shift
done

die() { echo "make-deb: $*" >&2; exit 1; }
say() { echo "==> $*"; }

command -v dpkg-deb >/dev/null || die "dpkg-deb not found (macOS: brew install dpkg)"
command -v dpkg-gencontrol >/dev/null || die "dpkg-gencontrol not found (it ships with dpkg)"

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

# --- the install layout ---------------------------------------------------
# The unit, the postinst and the daemon each name some of these paths, and
# nothing at runtime notices when they stop agreeing - it just quietly does the
# wrong thing: a config written where the unit does not look, or an outage's
# frames moved somewhere the daemon will not open. So each is compared against
# whichever artefact actually owns it, here, where a mismatch is a failed build.
REF_PATH=/usr/share/${PKG}/${PKG}.toml
DOC_DIR=/usr/share/doc/${PKG}
STAGED_CACHE_FILE=adopt.db
LEGACY_CACHE_FILE=.wiretap-server-cache.db

maint_var() {  # maint_var <script> <name> — the literal it is assigned
	sed -n "s/^$2=//p" "${DEBIAN}/$1" | head -1 | tr -d '"'
}
same() {  # same <what> <wanted> <got>
	[ "$2" = "$3" ] || die "the packaging disagrees about $1:
     wanted ${2}
     got    ${3}"
}

CONF_TOML="$(maint_var postinst CONFIG_DIR)/wiretap-server.toml"

# The changelog is installed into the package, so an entry naming a version no
# package ever has is documentation of a release that does not exist.
same "the changelog version" "${version}" \
	"$(dpkg-parsechangelog -l "${DEBIAN}/changelog" -S Version)"

if ! grep -qE '^ExecStart=/usr/bin/wiretap-server( |$)' "${UNIT}"; then
	die "packaging/${PKG}.service does not ExecStart /usr/bin/wiretap-server.
     The package installs to /usr/bin; a unit naming /usr/local/bin would
     produce a box whose daemon points at a path dpkg does not manage."
fi

# The unit passes -C because the daemon has NO default config path - it reads a
# file only when told to. Drop the flag and the package silently installs a
# configuration file that nothing ever opens, and the daemon runs on built-in
# defaults instead. Checked against the postinst's own value, so this asserts
# the unit reads the file that is actually written rather than a literal.
if ! grep -qF -- "-C ${CONF_TOML}" "${UNIT}"; then
	die "packaging/${PKG}.service does not pass -C ${CONF_TOML}.
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

# The state directory is the unit's to declare - StateDirectory= is what creates
# it and what the daemon defaults its cache from - so the postinst is checked
# against the unit rather than against a third copy here.
STATE_DIR="$(maint_var postinst STATE_DIR)"
same "the state directory" \
	"/var/lib/$(grep -E '^StateDirectory=' "${UNIT}" | head -1 | cut -d= -f2)" \
	"${STATE_DIR}"

same "the packaged reference config" "${REF_PATH}" "$(maint_var postinst REFERENCE_TOML)"
# Resolved rather than compared as text: the postinst spells this
# "$STATE_DIR/adopt.db", and a check on the string would fail a rewrite that
# meant exactly the same path.
same "the staged cache" "${STATE_DIR}/${STAGED_CACHE_FILE}" \
	"$(maint_var postinst STAGED_CACHE | sed "s|\$STATE_DIR|${STATE_DIR}|")"

# postrm removes what postinst writes, and neither would notice the other
# renaming it - a purged package that leaves the drop-in behind goes on forcing
# Storage=persistent on a host that no longer runs this.
# USER is in here for a sharper reason than the rest: one script chowns the
# state directory to it and the other decides whether to delete it, and a
# disagreement leaves a kept capture owned by an account nothing recreates.
for v in USER CONFIG_DIR STATE_DIR UNIT JOURNALD_DROPIN; do
	same "${v} between postinst and postrm" \
		"$(maint_var postinst "${v}")" "$(maint_var postrm "${v}")"
done

# Both cache names are the daemon's, not the packaging's: the postinst stages an
# outage's frames at a path only settings.rs looks for. Rename either there and
# the frames sit where nothing opens them, silently, after an upgrade mid-outage.
SETTINGS="${ROOT}/crates/wiretap-server/src/settings.rs"
for name in "${STAGED_CACHE_FILE}" "${LEGACY_CACHE_FILE}"; do
	grep -qF "\"${name}\"" "${SETTINGS}" \
		|| die "settings.rs no longer knows ${name}; debian/postinst still stages to it."
done

# A different coupling to the same file, and a different failure: this is the
# variable the documented --check-config command is prefixed with. The daemon
# derives the cache from it, so without it that command reports a path under
# $HOME the unit never opens and prints no "adopt on start" line.
grep -qF '"STATE_DIRECTORY"' "${SETTINGS}" \
	|| die "settings.rs no longer reads STATE_DIRECTORY; the packaging still tells
     operators to set it before --check-config."

# And the value, which the unit and the reference config hardcode rather than
# deriving - eight lines from the StateDirectory= they come from. Checked
# against ${STATE_DIR}, which is itself already checked against that line.
for f in "${UNIT}" "${CONFIG}"; do
	grep -qF "STATE_DIRECTORY=${STATE_DIR}" "${f}" \
		|| die "${f#"${ROOT}"/} does not prefix its --check-config advice with
     STATE_DIRECTORY=${STATE_DIR}, so it documents a command that reports a
     cache path the daemon does not use."
done

# One table rather than a function per column: an architecture is a rust target
# and the ELF e_machine that proves the target took, and splitting them means
# adding one edits two places that have to agree. (183 = AArch64, 62 = x86-64.)
rust_target() {
	case "$1" in
		arm64) echo aarch64-unknown-linux-musl ;;
		amd64) echo x86_64-unknown-linux-musl ;;
		*) die "no target mapping for architecture $1" ;;
	esac
}
elf_machine() {
	case "$1" in
		arm64) echo 183 ;;
		amd64) echo 62 ;;
		*) die "no e_machine mapping for architecture $1" ;;
	esac
}

# Every requested target, before any of them compiles. The tree-level checks
# above already follow this rule; a toolchain check inside the per-architecture
# loop would report a missing amd64 target only after arm64 had built, which is
# the whole build wasted to say something knowable up front.
if [ "${skip_build}" -eq 0 ]; then
	command -v cargo-zigbuild >/dev/null || die "cargo-zigbuild not found (brew install cargo-zigbuild zig)"
	installed="$(rustup target list --installed 2>/dev/null)"
	for a in "${arches[@]}"; do
		t="$(rust_target "${a}")"
		grep -qx "${t}" <<<"${installed}" \
			|| die "rust target ${t} not installed - rustup target add ${t}"
	done
fi

build_arch() {  # build_arch <arch>
	local arch="$1" target machine
	target="$(rust_target "${arch}")"
	machine="$(elf_machine "${arch}")"

	if [ "${skip_build}" -eq 0 ]; then
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
	# ELF has no PT_INTERP, so there is no interpreter string in it. `grep -a`
	# reads the binary directly, where `strings` would rebuild it as text first
	# to answer the same question.
	if grep -qa 'ld-linux' "${bin}"; then
		die "[${arch}] ${PKG} looks dynamically linked - the musl target did not take"
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

	# Control: generated from debian/control by dpkg's own tool, which merges the
	# source and binary stanzas, computes Installed-Size from -P, and drops the
	# source-only fields. Hand-assembling it with a list of `echo`s was a
	# whitelist, so any field added to debian/control that nobody remembered to
	# add here would be silently absent from the package - `Suggests:` very
	# nearly was. -v keeps the version coming from Cargo.toml rather than the
	# changelog, which is the policy this script has.
	# -f puts the .changes files list under target/ rather than letting it
	# default to debian/files: it is for dpkg-buildpackage, which is not how
	# this builds, and the default drops a stray artefact in the source tree.
	# (It cannot be /dev/null — dpkg-gencontrol writes `.new` and renames.)
	dpkg-gencontrol -p"${PKG}" -c"${DEBIAN}/control" -l"${DEBIAN}/changelog" \
	                -P"${stage}" -v"${version}" -DArchitecture="${arch}" \
	                -f"${ROOT}/target/deb/${PKG}.files"

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
