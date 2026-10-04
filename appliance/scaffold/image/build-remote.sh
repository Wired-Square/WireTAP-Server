#!/bin/sh
# Builds wiretap-appliance's SD-card image on the Linux build box, from here.
#
#     scaffold/image/build-remote.sh              build, then fetch the image
#     scaffold/image/build-remote.sh --no-fetch   build, and leave it there
#     scaffold/image/build-remote.sh --fetch      fetch the last build's image
#
# Chassis-owned, and rewritten unconditionally by every render. pi-gen wants
# Debian, root and loop devices, so the image is not built on a laptop: this
# sends `scaffold/` and the arm64 package — never the source tree — and runs
# `build-image.sh` there under sudo, detached, following its log. The box is
# `BUILD_HOST`, `BUILD_USER` and `BUILD_DIR`, from the environment or from
# `config.local` beside this.
set -eu
# The fleet key, as `op run` puts it in the environment, taken before
# config.local is sourced so the environment wins, and out of the environment
# every command below inherits.
unset ENV_KEY
ENV_KEY=${TAILSCALE_AUTH_KEY:-}
unset TAILSCALE_AUTH_KEY

NAME='wiretap-appliance'
DEB_PACKAGE='wiretap-appliance'
TAILSCALE=1

die() {
    echo "build-remote: $1" >&2
    exit 1
}
say() { echo "build-remote: $1"; }

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
SCAFFOLD=$(dirname -- "$HERE")
cd -- "$(dirname -- "$SCAFFOLD")"

build=1
fetch=1
for arg in "$@"; do
    case "$arg" in
    --no-fetch) fetch=0 ;;
    --fetch) build=0 ;;
    *) die "unknown argument: $arg" ;;
    esac
done
[ "$build" = 1 ] || [ "$fetch" = 1 ] || die "--fetch and --no-fetch together would do nothing"

# The character check is what makes the key safe single-quoted below; the
# message never shows it.
if [ "$build" = 1 ] && [ -n "$ENV_KEY" ]; then
    only "$ENV_KEY" _- ||
        die "TAILSCALE_AUTH_KEY in the environment holds a character other than A-Z, a-z, 0-9, _ and -, so it is not a plain auth key; put anything else in config.local"
    [ "$TAILSCALE" = 1 ] ||
        die "TAILSCALE_AUTH_KEY is in the environment, but appliance.toml has no [targets.image.tailscale] to enrol with"
fi

build_box
command -v rsync >/dev/null || die "rsync is not on PATH"
say "build box $TARGET, tree at $BUILD_DIR"

# Every ssh and rsync rides one connection, opened while the agent answers, so
# a 1Password agent that locks during the build does not strand the image. The
# socket's directory is this run's alone, and /tmp keeps its path inside a unix
# socket's limit.
connect() {
    MUX=$(mktemp -d /tmp/build-remote.XXXXXX)
    SSH="ssh -o ControlPath=$MUX/%C -o ConnectTimeout=10"
    # ssh takes an option's first value, so ControlMaster is set here, where it
    # outranks a `ControlMaster yes` in ~/.ssh/config that would stop a client
    # reusing the master.
    RSH="$SSH -o ControlMaster=no"
    trap '$SSH -O exit "$TARGET" 2>/dev/null || true; rm -rf "$MUX"' EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    $SSH -o ControlMaster=yes -fN -o ServerAliveInterval=30 "$TARGET" >/dev/null ||
        die "cannot ssh to $TARGET"
}
box() { $RSH "$@"; }

# Under `set -e`, a bare ssh or rsync failure once the build has started would
# exit with the image built and unfetched.
lost() {
    [ "$build" = 0 ] || die "$1 — the build may have finished on $BUILD_HOST; rerun with --fetch to collect it"
    die "$1 — is $BUILD_HOST reachable, and the ssh agent unlocked?"
}

# `build-image.sh` run on the box by hand leaves an earlier build's status in
# place while it rewrites the image, so a running one outranks the status.
finished() {
    status=$(box -n "$TARGET" "if pgrep -f '[b]uild-image.sh' >/dev/null; then echo running; else cat '$BUILD_DIR/build.status' 2>/dev/null || echo none; fi") ||
        lost "could not read $BUILD_DIR/build.status"
    case $status in
    0) say "build finished cleanly" ;;
    running) die "a build is still running on $BUILD_HOST; its log is $BUILD_DIR/build.log, and --fetch collects it once it has finished" ;;
    none) die "$BUILD_HOST has no finished build and none running; see $BUILD_DIR/build.log if one was started" ;;
    '' | *[!0-9]*) die "$BUILD_DIR/build.status on $BUILD_HOST holds no exit code; see $BUILD_DIR/build.log" ;;
    *) die "the build failed (exit $status); see $TARGET:$BUILD_DIR/build.log" ;;
    esac
}

fetch_image() {
    mkdir -p "$HERE/deploy"
    # `-a` keeps the 0600 build-image.sh gave an image built with the fleet key.
    rsync -a --progress -e "$RSH" "$TARGET:$BUILD_DIR/scaffold/image/pi-gen/deploy/" "$HERE/deploy/" ||
        lost "could not fetch the image"
    img=$(ls -t "$HERE"/deploy/*.img.xz 2>/dev/null | head -1)
    [ -z "$img" ] || say "sha256 $(basename "$img"): $(shasum -a 256 "$img" | cut -d' ' -f1)"
}

if [ "$build" = 0 ]; then
    connect
    finished
    fetch_image
    exit 0
fi

# The same package `build-image.sh` will look for, checked here so the refusal
# is a second and not a round trip.
DEB=$(ls -t target/debian/image/"${DEB_PACKAGE}"_*_arm64.deb 2>/dev/null | head -1)
[ -n "$DEB" ] || die "no target/debian/image/${DEB_PACKAGE}_*_arm64.deb — run scaffold/packaging/make-deb.sh --target image first"

connect
box "$TARGET" 'command -v git >/dev/null && command -v sudo >/dev/null && command -v pgrep >/dev/null' ||
    die "$TARGET lacks git, sudo or pgrep"

# A build still running there is refused — the process, not the log, since a
# box that rebooted mid-build has the log and no status forever. Its own ssh,
# because sshd runs a string as one shell's command line and the start line
# spells the script's name, which the bracket cannot hide.
box -n "$TARGET" "! pgrep -f '[b]uild-image.sh' >/dev/null" ||
    die "a build is still running on $BUILD_HOST — if this script started it, its log is $BUILD_DIR/build.log"

# The pi-gen clone and its output belong to the build box. The previous
# package goes first: `build-image.sh` takes the newest by mtime, which
# `rsync -a` preserves, so a stale one could outrank this.
say "sending scaffold/ and $(basename "$DEB")"
box "$TARGET" "mkdir -p '$BUILD_DIR/target/debian/image' && rm -f '$BUILD_DIR'/target/debian/image/${DEB_PACKAGE}_*_arm64.deb"
rsync -az --delete -e "$RSH" \
    --exclude 'image/pi-gen/' --exclude 'image/deploy/' --exclude 'image/config.local' \
    --exclude 'image/stage-appliance/00-appliance/files/appliance.deb' \
    "$SCAFFOLD/" "$TARGET:$BUILD_DIR/scaffold/"
rsync -a -e "$RSH" "$DEB" "$TARGET:$BUILD_DIR/target/debian/image/"
# config.local may hold the fleet key, so it lands readable by its owner alone
# whatever its mode here. Not `rsync --chmod`, which macOS's openrsync ignores.
# The environment's key follows the file, so it wins, and goes through stdin:
# `printf` is a builtin in sh and dash, so the key is in no process's argv.
{
    [ ! -f "$HERE/config.local" ] || cat "$HERE/config.local"
    [ -z "$ENV_KEY" ] || printf "\nTAILSCALE_AUTH_KEY='%s'\n" "$ENV_KEY"
} | box "$TARGET" "umask 077 && rm -f '$BUILD_DIR/scaffold/image/config.local' && cat > '$BUILD_DIR/scaffold/image/config.local'"

# Detached under setsid, so the build survives losing this terminal; the
# status file is how the exit code comes back. **The `&` has to be on the
# `setsid` command alone**: on an AND-list the shell backgrounds the whole list
# in a subshell that keeps sshd's pipes until the build ends, so the ssh never
# returns and a dropped connection kills this script with the build still
# running.
say "building on $BUILD_HOST — this is the long part"
box -n "$TARGET" "cd '$BUILD_DIR' || exit 1
    rm -f build.log build.status
    setsid sh -c 'sudo scaffold/image/build-image.sh > build.log 2>&1; echo \$? > build.status' > /dev/null 2>&1 < /dev/null &" ||
    die "could not start the build on $TARGET"
say "following $BUILD_DIR/build.log"
box -n "$TARGET" "cd '$BUILD_DIR' || exit 1
    tail -n +1 -F build.log 2>/dev/null &
    while [ ! -f build.status ]; do sleep 5; done
    sleep 2; kill \$! 2>/dev/null" || true

finished
if [ "$fetch" = 1 ]; then
    fetch_image
else
    say "left on $BUILD_HOST at $BUILD_DIR/scaffold/image/pi-gen/deploy"
fi
