#!/bin/sh
#
# Print the build id for the current checkout: `g<12-char sha>`, plus `-dirty`
# when tracked files have been edited.
#
# This is the shell counterpart of `wiretap_build_id::git_id`, and exists for
# the same reason that crate does — the format is the thing two implementations
# drift on, and the `-dirty` half is the half that gets dropped. A gateway image
# built from an edited tree and stamped `g<sha>` claims a commit it is not from,
# which is worse than claiming nothing: `make-deb.sh` refuses to ship exactly
# that for the .deb, by grepping the built binary for the whole stamp.
#
# The container build needs this because there is no `.git` inside it to ask —
# hence `--build-arg WIRETAP_BUILD_ID=$(packaging/build-id.sh)`.
#
# `make-deb.sh` computes the same thing inline rather than calling this: it
# needs the pieces separately (the commit's date, the sha without the `g`, and
# `.dirty` spelled dpkg's way), so sharing would mean returning three values to
# save one line. The rule they must agree on — 12 characters, `g` prefix,
# `-dirty` from tracked files only — is stated here and asserted there.
#
# CI does not use this: a runner's checkout is clean by construction and the
# workflows already have the sha in `$GITHUB_SHA`.

set -eu

cd "$(dirname "$0")/.."

sha="$(git rev-parse --short=12 HEAD 2>/dev/null || true)"
if [ -z "${sha}" ]; then
	echo "build-id: not a git checkout — pass WIRETAP_BUILD_ID yourself, or accept 'unknown'" >&2
	exit 1
fi

# Tracked files only, matching git_id(): an untracked scratch file is not a
# modification of the commit, and counting it would mark almost every working
# tree dirty.
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
	printf 'g%s-dirty\n' "${sha}"
else
	printf 'g%s\n' "${sha}"
fi
