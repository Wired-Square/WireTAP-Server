#!/bin/sh
# Removes what building wiretap-appliance's image left behind: the pi-gen clone, work
# directory included (`build-image.sh` reuses a clone rather than re-cloning,
# so a stale one builds from the wrong pi-gen ref), and the staged package.
# Built images are kept unless asked.
#
#     scaffold/image/clean.sh              here
#     scaffold/image/clean.sh --remote     on the build box build-remote.sh uses
#     scaffold/image/clean.sh --images     the built images too
#
# Chassis-owned, and rewritten unconditionally by every render.
set -eu
unset TAILSCALE_AUTH_KEY

NAME='wiretap-appliance'

die() {
    echo "clean: $1" >&2
    exit 1
}

# Whether $1 holds only letters, digits and the characters in $2. Spelled out,
# because in macOS's sh a range in a pattern follows the locale's collation.
only() {
    case $1 in *[!ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789$2]*) return 1 ;; esac
}

# The build box, from the environment over config.local. Each value must stay
# one word through ssh, rsync and the '…' strings of the remote commands, and
# a refusal names the variable, never its value.
build_box() {
    unset ENV_HOST ENV_USER ENV_DIR
    ENV_HOST=${BUILD_HOST:-} ENV_USER=${BUILD_USER:-} ENV_DIR=${BUILD_DIR:-}
    [ ! -f "$HERE/config.local" ] || . "$HERE/config.local"
    unset TAILSCALE_AUTH_KEY
    BUILD_HOST=${ENV_HOST:-${BUILD_HOST:-}}
    BUILD_USER=${ENV_USER:-${BUILD_USER:-root}}
    BUILD_DIR=${ENV_DIR:-${BUILD_DIR:-/root/$NAME}}
    [ -n "$BUILD_HOST" ] || die "BUILD_HOST is set neither in the environment nor in scaffold/image/config.local"
    case $BUILD_HOST in -*) die "BUILD_HOST must not start with -" ;; esac
    case $BUILD_USER in -*) die "BUILD_USER must not start with -" ;; esac
    only "$BUILD_HOST" ._- ||
        die "BUILD_HOST may hold only A-Z, a-z, 0-9, ., _ and - (an IPv6 box wants a Host alias in ~/.ssh/config; a user goes in BUILD_USER)"
    only "$BUILD_USER" ._- || die "BUILD_USER may hold only A-Z, a-z, 0-9, ., _ and -"
    case $BUILD_DIR in
    / | [!/]* | */. | */./* | */.. | */../* | *//*)
        die "BUILD_DIR must be an absolute path other than /, with no ., .. or empty component" ;;
    esac
    only "$BUILD_DIR" ._/- || die "BUILD_DIR may hold only A-Z, a-z, 0-9, ., _, / and -"
    TARGET="$BUILD_USER@$BUILD_HOST"
}

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

remote=0
images=0
for arg in "$@"; do
    case "$arg" in
    --remote) remote=1 ;;
    --images) images=1 ;;
    *) die "unknown argument: $arg" ;;
    esac
done

# One body, run here or sent over ssh, so the two cannot drift. `pi-gen/deploy`
# is where the images are, and it goes with the clone: they are moved aside
# first unless --images says otherwise.
body() {
    cat <<'BODY'
set -eu
IMAGE=$1; images=$2
# pi-gen bind-mounts /dev and /sys under its work tree and unmounts on exit;
# a build killed outright leaves them, and `rm -r` does not stop at a mount.
! grep -q " $IMAGE/pi-gen/" /proc/mounts 2>/dev/null ||
    { echo "clean: something is still mounted under $IMAGE/pi-gen — a build is running, or died with its mounts up; unmount first" >&2; exit 1; }
if [ "$images" = 0 ] && [ -n "$(ls -A "$IMAGE/pi-gen/deploy" 2>/dev/null)" ]; then
    mkdir -p "$IMAGE/deploy"
    mv "$IMAGE/pi-gen/deploy"/* "$IMAGE/deploy/"
fi
rm -rf "$IMAGE/pi-gen" "$IMAGE/stage-appliance/00-appliance/files/appliance.deb"
[ "$images" = 0 ] || rm -rf "$IMAGE/deploy"
echo "clean: $IMAGE"
BODY
}

if [ "$remote" = 1 ]; then
    build_box
    body | ssh "$TARGET" "sh -s -- '$BUILD_DIR/scaffold/image' $images"
else
    body | sh -s -- "$HERE" "$images"
fi
