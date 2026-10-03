#!/bin/sh
# Prints the identity of the build happening in this checkout, for wiretap-appliance.
#
# Chassis-owned, and rewritten unconditionally by every render.
#
# **This exists for the build that cannot read git for itself** — a container
# whose context carries no .git, an unpacked source tarball, a chroot. Run it
# where git is and carry the value to where git is not:
#
#   APPLIANCE_BUILD_ID=$(./scaffold/packaging/build-id.sh)
#   export APPLIANCE_BUILD_ID
#
#   docker build --build-arg APPLIANCE_BUILD_ID="$(./scaffold/packaging/build-id.sh)" .
#
# `appliance-build-id` reads that variable *before* the working tree, which is
# the opposite of the obvious ordering and is deliberate: a container build often
# does have a .git in its context, and it belongs to whatever the build host
# happened to check out rather than to what is being built. Somebody who set the
# variable knows something the working tree does not.
#
# **It computes nothing.** The format lives in one crate, and a shell
# reimplementation of it is how a package's version and its binary's --version
# stop agreeing — which is the failure the whole crate exists to prevent.
#
# It refuses rather than printing the literal `unknown`, so a build with nothing
# to name it fails here instead of at `make-deb.sh`, or worse, in the field.
#
# Note what this value is *not* good for: a build id supplied this way is used
# verbatim as the --version stamp and carries no commit date, so `make-deb.sh`
# cannot assemble a sorting Debian version from it. Package from a checkout.
set -eu

command -v appliance-xtask >/dev/null || {
    echo "build-id: appliance-xtask is not on PATH" >&2
    exit 1
}

exec appliance-xtask build-id
