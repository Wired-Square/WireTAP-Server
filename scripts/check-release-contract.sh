#!/usr/bin/env bash
#
# Assert the things a release depends on that a release would not otherwise
# notice were false.
#
# Three files have to agree about what version this is — the git tag,
# `[workspace.package] version` in Cargo.toml, and debian/changelog — and each is
# checked by something different, at a different time:
#
#   * make-deb.sh compares Cargo.toml against debian/changelog, at build time.
#   * release.yml compares the tag against Cargo.toml, after the tag exists.
#
# Which leaves a gap exactly where it hurts. By the time either of those fires
# the tag has been pushed, and a tag is the one part of a release that cannot be
# corrected in place. So the same comparison is made here, before the commit.
#
# Run by the pre-release hook AND by CI, which is the only way it can catch the
# hook being deleted rather than merely weakened — nothing invoked by a hook can
# police its own existence.
#
# Safe to run by hand at any time.

set -euo pipefail

cd "$(dirname "$0")/.."

fail() { printf '%s\n' "$*" >&2; exit 1; }

# release.toml is parsed rather than pattern-matched: `pre-release-hook` is an
# array *or* a string and may be written across several lines, and a comment
# merely mentioning `cargo test` must not be mistaken for a hook that runs it.
eval "$(python3 - <<'PY'
import shlex, sys, tomllib

try:
    cfg = tomllib.load(open("release.toml", "rb"))
except FileNotFoundError:
    sys.exit("release.toml is missing; the release path is defined by it")
except tomllib.TOMLDecodeError as e:
    sys.exit(f"release.toml does not parse: {e}")

hook = cfg.get("pre-release-hook", "")
if isinstance(hook, list):
    hook = " ".join(hook)

# No braces containing a comma anywhere below: this block reaches bash before it
# reaches Python, and bash brace-expands `{a, b}` into two words — which silently
# runs this four times and feeds eval the uninterpolated source.
for key, want in (("publish", False), ("push", False), ("shared-version", True)):
    var = key.replace("-", "_")
    print("cfg_" + var + "=" + shlex.quote(str(cfg.get(key))))
    print("want_" + var + "=" + shlex.quote(str(want)))

print("cfg_hook=" + shlex.quote(hook))
print("cfg_tag_name=" + shlex.quote(str(cfg.get("tag-name"))))
PY
)"

# --- release.toml still keeps its promises ------------------------------------

[ "$cfg_publish" = "$want_publish" ] ||
	fail "release.toml: publish is ${cfg_publish}, want ${want_publish} — nothing here goes to crates.io"

# The one that is easy to 'tidy' into true, and doing so races CI: release.yml
# refuses a commit with no successful ci.yml run, and ci.yml never runs on a tag.
[ "$cfg_push" = "$want_push" ] ||
	fail "release.toml: push is ${cfg_push}, want ${want_push} — the tag must follow a green CI run, not travel with it"

[ "$cfg_shared_version" = "$want_shared_version" ] ||
	fail "release.toml: shared-version is ${cfg_shared_version}, want ${want_shared_version} — one tag covers every crate"

[ "$cfg_tag_name" = 'v{{version}}' ] ||
	fail "release.toml: tag-name is ${cfg_tag_name}, want v{{version}} — release.yml triggers on 'v*' and strips the v"

for gate in \
	'cargo fmt' \
	'cargo clippy --workspace' \
	'aarch64-unknown-linux-musl' \
	'cargo test --workspace'
do
	case "$cfg_hook" in
		*"$gate"*) ;;
		*) fail "release.toml: the pre-release hook no longer runs '${gate}'" ;;
	esac
done

# --- the three versions agree -------------------------------------------------

cargo_version="$(awk '/^\[workspace\.package\]/{f=1} f && /^version[[:space:]]*=/{gsub(/[",]/,"",$3); print $3; exit}' Cargo.toml)"
[ -n "$cargo_version" ] || fail "could not read [workspace.package] version from Cargo.toml"

if command -v dpkg-parsechangelog >/dev/null 2>&1; then
	deb_version="$(dpkg-parsechangelog -l debian/changelog -S Version)"
else
	# Same field, without the Debian tooling — this runs on macOS too.
	deb_version="$(sed -n '1s/^[^(]*(\([^)]*\)).*/\1/p' debian/changelog)"
fi
[ -n "$deb_version" ] || fail "could not read a version from debian/changelog"

# debian/changelog carries the packaging revision, so 0.1.0 and 0.1.0-2 are both
# releases of 0.1.0. Compare the upstream part, which is what must match.
[ "${deb_version%%-*}" = "$cargo_version" ] || fail "$(cat <<EOF
debian/changelog names ${deb_version}, Cargo.toml names ${cargo_version}.

Add the entry for ${cargo_version} to debian/changelog before releasing. It is
prose about an upgrade path and nothing can generate it; make-deb.sh would
otherwise refuse to build the package, after the tag had been cut.
EOF
)"

printf 'release contract OK — version %s, three files agree\n' "$cargo_version"
