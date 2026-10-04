#!/bin/sh
# Builds wiretap-appliance's SD-card image with pi-gen.
#
# Chassis-owned, and rewritten unconditionally by every render. A product's own
# image steps go in a numbered substage beside this stage's, which nothing here
# writes — see the stage directory's own README-shaped comments.
set -eu

NAME='wiretap-appliance'
DEB_PACKAGE='wiretap-appliance'
PI_GEN_REF='arm64'
TAILSCALE=1
# `config.local` is sourced below, and a reassignment there must abort the
# build rather than stage the fleet key for a product without Tailscale.
readonly TAILSCALE

die() {
    echo "build-image: $1" >&2
    exit 1
}

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SCAFFOLD=$(dirname -- "$HERE")
cd -- "$(dirname -- "$SCAFFOLD")"

# **pi-gen only runs on Debian**, and on the same architecture it is building
# for unless you like waiting on qemu. This is the one script in the scaffold
# that is not expected to run on a developer's laptop.
[ "$(uname -s)" = Linux ] || die "pi-gen builds on Debian; this is $(uname -s)"
command -v git >/dev/null || die "git is not on PATH"

# **Checked here rather than left to pi-gen.** pi-gen refuses early in its own
# `build.sh`, but this script does not reach that until its last line — so the
# refusal would land after the clone, the stage copy and the package copy.
# Failing before the clone is the difference between a message and a wasted
# download.
[ "$(id -u)" = 0 ] || die "pi-gen builds as root; re-run this under sudo"

# The fleet key comes from config.local alone: sourcing it below would stage
# one from the environment too.
[ -z "${TAILSCALE_AUTH_KEY:-}" ] ||
    die "TAILSCALE_AUTH_KEY is in the environment, but a local build reads it only from config.local: put it there, or build with build-remote.sh"

# A pipeline's status is `head`'s, which always succeeds, so the emptiness test
# below is the guard rather than a `|| die` on the assignment.
DEB=$(ls -t target/debian/image/"${DEB_PACKAGE}"_*_arm64.deb 2>/dev/null | head -1)
[ -n "$DEB" ] ||
    die "no arm64 package in target/debian/image — run scaffold/packaging/make-deb.sh --target image first"

# **Pinned to a ref rather than tracking a branch.** pi-gen's stages change, and
# an image that built last month and does not today is a day spent bisecting
# somebody else's repository. `appliance.toml` carries the ref so moving it is a
# recorded decision.
if [ ! -d "$HERE/pi-gen" ]; then
    git clone --depth 1 --branch "$PI_GEN_REF" \
        https://github.com/RPi-Distro/pi-gen.git "$HERE/pi-gen"
fi

# The chassis's stage, copied in rather than symlinked: pi-gen mounts and
# chroots under its own directory, and a symlink out of it is a path that does
# not resolve inside.
rm -rf "$HERE/pi-gen/stage-appliance"
cp -a "$HERE/stage-appliance" "$HERE/pi-gen/stage-appliance"

# The package the stage installs, staged where the chroot can see it.
mkdir -p "$HERE/pi-gen/stage-appliance/00-appliance/files"
cp "$DEB" "$HERE/pi-gen/stage-appliance/00-appliance/files/appliance.deb"

# config.local's fleet key, staged below and removed when this script ends,
# unless it is killed with SIGKILL. dash runs an EXIT trap on `exit` but not
# on a fatal signal, hence the signal traps.
STAGED_KEY="$HERE/pi-gen/stage-appliance/00-appliance/files/tailscale.key"
readonly STAGED_KEY
trap 'rm -f "$STAGED_KEY"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# **stage2 carries an `EXPORT_IMAGE` of its own**, so without this pi-gen writes
# a lite image beside the appliance one: two files in `deploy/`, twice the
# export time, and a card somebody can write by mistake. `SKIP_IMAGES` is read
# relative to each stage directory, which is why it is a file and not a setting.
touch "$HERE/pi-gen/stage2/SKIP_IMAGES"

# ---------------------------------------------------------------------------
# Two substages this appliance does not want, skipped with pi-gen's own marker
# ---------------------------------------------------------------------------
# A `SKIP` file makes `run_sub_stage` step over a substage entirely — packages
# and all — which is what both of these need and what neither `config` setting
# gives.
#
# **The console account wizard.** `export-image/01-user-rename` runs
# `rename-user -f -s` in the image, which disables `getty@tty1` and enables
# `userconfig.service`. On a lite image that service comes up on a spare VT with
# no credential required and creates an account in the `sudo` group. An
# appliance that ships `ENABLE_SSH=0` and no password, and then offers anyone
# with a keyboard a root-capable account, has not shipped without credentials —
# it has shipped with one anybody can mint. Skipping the substage also leaves
# `getty@tty1` enabled, so an operator who does configure a login still has a
# console.
#
# `DISABLE_FIRST_BOOT_USER_RENAME=1` would also turn it off, and is not used:
# `build.sh` refuses to start unless `FIRST_USER_PASS` is set alongside it,
# which is a password shipped on every card. This reaches the same place
# without paying that.
skip() {
    [ -d "$HERE/pi-gen/$1" ] || die "pi-gen no longer has $1; the skip would touch nothing"
    touch "$HERE/pi-gen/$1/SKIP"
}
skip export-image/01-user-rename

# **cloud-init**, which is in stage2 rather than export-image.
# `ENABLE_CLOUD_INIT=0` would stop the seeding but not the install — the guard
# is in `01-run.sh` and the package list is `00-packages`, which runs first. An appliance does not want an agent that reads `user-data`
# off the FAT boot partition, where anyone holding the card can add a user, set
# a password or run a command; and it is a second authority over `/etc/hostname`
# beside the first-boot unit. A product that wants cloud-init deletes this line
# by asking for the knob.
skip stage2/04-cloud-init

cp "$HERE/config" "$HERE/pi-gen/config"
chmod 0600 "$HERE/pi-gen/config"

# `config.local` is a consumer's, never the chassis's, and is gitignored: it is
# where a deploy target or a developer's key goes. Sourced after the rendered
# config so it wins.
if [ -f "$HERE/config.local" ]; then
    cat "$HERE/config.local" >> "$HERE/pi-gen/config"
    # A key from here is a developer's card, and the image says so in its
    # name — read by sourcing, so `export X=` and `X=` both count.
    ( . "$HERE/config.local"; [ -z "${PUBKEY_SSH_ROOT:-}${PUBKEY_SSH_FIRST_USER:-}" ] ) ||
        printf "IMG_NAME='%s-dev'\n" "$NAME" >> "$HERE/pi-gen/config"

    # The fleet key reaches the stage as a file, and pi-gen never sees the
    # name: an `export` in config.local would put it in the environment of
    # every process pi-gen starts.
    printf 'unset TAILSCALE_AUTH_KEY\n' >> "$HERE/pi-gen/config"
    (
        . "$HERE/config.local"
        [ -n "${TAILSCALE_AUTH_KEY:-}" ] || exit 0
        [ "$TAILSCALE" = 1 ] ||
            die "TAILSCALE_AUTH_KEY is set in config.local, but appliance.toml has no [targets.image.tailscale] to enrol with"
        [ -z "${DEPLOY_DIR:-}${WORK_DIR:-}" ] ||
            die "DEPLOY_DIR or WORK_DIR is set in config.local beside TAILSCALE_AUTH_KEY — an image holding the key goes only where this script can make it private"
        umask 077
        printf '%s\n' "$TAILSCALE_AUTH_KEY" > "$STAGED_KEY"
    )
fi

# Read back the way pi-gen reads it, so the refusal is about what the build
# would see. A password is one every card shares; the verify substage refuses
# it as well, once every stage has run.
(
    . "$HERE/pi-gen/config"
    [ -z "${FIRST_USER_PASS:-}" ] ||
        die "FIRST_USER_PASS is set in config.local — a password is the same credential on every card; use a key"
)

echo "build-image: building $NAME from $(basename "$DEB")"
cd "$HERE/pi-gen"
# An image built with the fleet key is as secret as the key: xz writes the
# image 0644, and the uncompressed one stays in work/.
if [ -f "$STAGED_KEY" ]; then
    mkdir -p deploy work
    chmod 0700 deploy work
fi
./build.sh
if [ -f "$STAGED_KEY" ]; then
    find deploy -maxdepth 1 -type f -exec chmod 0600 {} +
    # So the account that ran sudo can fetch them, as build-remote.sh does.
    [ -z "${SUDO_UID:-}" ] || chown -R "$SUDO_UID:${SUDO_GID:-$SUDO_UID}" deploy
fi
