#!/bin/sh
# Builds wiretap-appliance's .deb for one target, from the files `appliance-xtask
# packaging render` wrote beside this one.
#
#     scaffold/packaging/make-deb.sh --target image
#
# Chassis-owned, and rewritten unconditionally by every render. **There is no
# hook for the build itself, and that is deliberate**: this runs on a build host
# and has no deployment-time behaviour to extend. What a product ships beyond the
# binary is named in appliance.toml — `hooks_dir`, `[package.files]`, `journal` —
# or placed where this script looks for it: the unit's `.service.d/*.conf`
# directory and the `debian/copyright` file. Anything else is a knob missing from
# appliance.toml.
set -eu

NAME='wiretap-appliance'
BINARY='wiretap-appliance'
MIN_BINARY_BYTES='14000000'

# The cargo package the `--bin` target lives in. The two are the same name in
# most products, and `cargo pkgid` fails loudly when they are not.
PACKAGE="${PACKAGE:-$BINARY}"

die() {
    echo "make-deb: $1" >&2
    exit 1
}

warn() {
    echo "make-deb: warning: $1" >&2
}

[ "$#" = 2 ] && [ "$1" = --target ] || die "usage: $0 --target image"
TARGET=$2
case "$TARGET" in
image) ;;
*) die "appliance.toml declares no target $TARGET — image" ;;
esac

# The Rust target to build, and below, the Debian architecture it produces. A
# card is a static musl binary on 64-bit ARM; the override exists so a package
# can be built for a test VM without editing a chassis-owned file. The generic
# package has no default: a host the product does not own may be either
# architecture, so it is named. It takes cargo's environment variable name;
# `build.target` in a cargo config is not read.
case "$TARGET" in
deb)
    [ -n "${CARGO_BUILD_TARGET:-}" ] ||
        die "--target deb needs CARGO_BUILD_TARGET set: x86_64-unknown-linux-musl for amd64, aarch64-unknown-linux-musl for arm64"
    ;;
esac
CARGO_BUILD_TARGET="${CARGO_BUILD_TARGET:-aarch64-unknown-linux-musl}"

# Addressed from this script's own location, so it runs from anywhere:
# scaffold/packaging/make-deb.sh -> scaffold -> the repo root, which is where
# cargo is invoked.
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SCAFFOLD=$(dirname -- "$HERE")
cd -- "$(dirname -- "$SCAFFOLD")"

TARGET_SCAFFOLD="$SCAFFOLD/targets/$TARGET"
CONTROL_IN="$TARGET_SCAFFOLD/debian/control.in"
UNIT="$TARGET_SCAFFOLD/systemd/$NAME.service"
# Under the product root, whatever `CARGO_TARGET_DIR` says, and one directory
# per target: the image scripts take the package from `target/debian/image`,
# so a generic build can never be the one that lands on a card.
OUT="target/debian/$TARGET"

# Passed to cargo rather than read back: a workspace member's default is the
# workspace's `target/`, not this product's.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

# The build host is a Debian-ish one — `dpkg-deb` says so, and `install -D`,
# `du --exclude` and `date -R` below are GNU rather than POSIX.
for tool in cargo dpkg-deb appliance-xtask; do
    command -v "$tool" >/dev/null || die "$tool is not on PATH"
done

# ---------------------------------------------------------------------------
# What is checked before anything is compiled
# ---------------------------------------------------------------------------
# A cross-build is minutes. Discovering afterwards that the unit starts a binary
# this package does not contain is discovering it slowly, so everything that can
# be answered from the rendered files is answered here.

[ -f "$CONTROL_IN" ] || die "$CONTROL_IN is not there — run \`appliance-xtask packaging render\`"
[ -f "$UNIT" ] || die "$UNIT is not there — run \`appliance-xtask packaging render\`"

# The unit and this script are rendered from one `binary` field, so a
# disagreement means one of them was edited — which is the question
# `appliance-xtask packaging diff` answers. `-x -F` rather than an anchored
# pattern, because a binary name legitimately contains `.` and `-`.
grep -q -x -F "ExecStart=/usr/bin/$BINARY" "$UNIT" ||
    die "$UNIT does not start /usr/bin/$BINARY — the unit and appliance.toml disagree"

# The three holes this script exists to fill. Checked rather than assumed: if
# the chassis stopped emitting one, the substitution below would still succeed
# and produce a control file with no version in it, which is a gate that has
# quietly stopped matching anything.
for token in '@VERSION@' '@ARCH@' '@INSTALLED_SIZE@'; do
    grep -q -F "$token" "$CONTROL_IN" ||
        die "$CONTROL_IN has no $token — it came from a chassis this script does not match"
done

# The maintainer, which is also the changelog's trailer. The example
# appliance.toml carries a placeholder so a product has to name its own, and a
# package that still carries it is one nobody can send a bug report about.
MAINTAINER=$(sed -n 's/^Maintainer: //p' "$CONTROL_IN")
case "$MAINTAINER" in
*example.invalid*) die "package.maintainer in appliance.toml is still the example's placeholder — name this product's own maintainer" ;;
esac

# The bundle, checked here for the reason the unit is: a cross-build is minutes, and
# finding out afterwards that there was nothing to package is finding out slowly.
UI_DIST='frontend/dist'
[ -d "$UI_DIST" ] ||
    die "$UI_DIST is not there — build the browser bundle first, or remove ui_dist from appliance.toml if this appliance has no browser half"

# **index.html is what every client-side route is answered with**, because the
# routes the browser shows exist only in the bundle and a plain file server 404s
# all of them. A directory without one is not a built bundle, and what it makes
# on a board is an appliance whose API works perfectly and whose every page is a
# 404.
[ -f "$UI_DIST/index.html" ] ||
    die "$UI_DIST has no index.html — that is what the daemon answers every page route with, so this is not a built bundle"

# **A source directory passes both checks above.** A product's vite root has an
# index.html by construction, so `ui_dist = "ui"` where `ui/dist` was meant ships
# `.env`, `.env.local` and every source file into a directory the appliance
# serves to anybody who can reach the port, with no credential. These three
# markers do not appear in build output and are conclusive when they do appear.
#
# Each name is quoted, and `.git` has to be: `neither_script_reimplements_the_
# version_format` greps these scripts for `git ` to catch one working out its own
# version in shell, and an unquoted `-name .git -o` is that string. Quoting is
# right anyway; the gate is why it is not optional.
SOURCEY=$(find "$UI_DIST/." \( -name '.git' -o -name 'node_modules' -o -name '.env*' \) )
[ -z "$SOURCEY" ] ||
    die "$UI_DIST looks like a source directory rather than build output, and everything in it would be served unauthenticated — name the directory your bundler writes: $SOURCEY"

# **Plain files and directories, and nothing else.** A symlink is the member
# everybody thinks of: `cp -R` recreates one rather than following it, dpkg ships
# it, and the daemon follows it on the board — tower-http's ServeDir has no
# symlink handling of any kind, it builds the path a component at a time,
# refusing `..` and absolute components, then opens it, which follows links the
# way open(2) does. So a link here is a served path resolving somewhere nobody
# packaged, or nowhere at all. Copying with `-L` instead would be worse: it pulls
# whatever the link points at into the package.
#
# **The test is the whole class rather than that one member**, because the
# sentence above was written as "a bundle is plain files" while the check said
# `-type l`, and a FIFO slips between the two: `cp -R` recreates it, both chmod
# passes skip it, the md5sums pass skips it, and `ServeDir` opening it for read
# blocks that request for ever. One predicate that matches the prose.
#
# `$UI_DIST/.` and not `$UI_DIST`, because find does not follow the starting
# operand — so the bare form reports a bundle directory that is *itself* a
# symlink as its own contents, which an ordinary monorepo layout produces.
ODD=$(find "$UI_DIST/." ! -type f ! -type d)
[ -z "$ODD" ] ||
    die "$UI_DIST holds something that is not a plain file or a directory, and a bundle is plain files — a symlink the appliance's file server would follow off the tree, or a FIFO that would hang the request that opened it: $ODD"

# **Names dpkg's md5sums can carry.** That file is one record per line, and GNU
# md5sum escapes a name holding a backslash by prefixing the whole line with `\`
# — which dpkg refuses with "control file 'md5sums' … is missing value
# separator", killing `dpkg --verify` for the *entire package* rather than for
# that one file. It is silent at build time and loud on the board, which is the
# wrong way round, so it is refused here where the message can name the file.
BACKSLASHED=$(find "$UI_DIST/." -name '*\\*')
[ -z "$BACKSLASHED" ] ||
    die "a name in $UI_DIST has a backslash in it, which dpkg's md5sums format cannot carry — rename it: $BACKSLASHED"

# A newline is the other name that format cannot carry, and it is the one that
# would make the md5sums count *lie* rather than fail — that count is `wc -l`.
# Counted rather than matched, because there is no `-name` pattern for a newline:
# one line per entry only holds while no entry has one in it.
LINES=$(find "$UI_DIST/." | wc -l | tr -d '[:space:]')
ENTRIES=$(find "$UI_DIST/." -print0 | tr -cd '\000' | wc -c | tr -d '[:space:]')
[ "$LINES" -eq "$ENTRIES" ] ||
    die "a name in $UI_DIST has a newline in it, which dpkg's md5sums format cannot carry"

# **A `.gz` is sent in place of the file beside it** to every browser that takes
# gzip, so it has to be that file. A build step that edits the output after the
# `.gz` was written leaves the old copy. A `.gz` with nothing beside it is a file
# of its own and ships as one.
STALE=$(find "$UI_DIST/." -type f -name '*.gz' | while IFS= read -r gz; do
    [ ! -f "${gz%.gz}" ] || gzip -dc -- "$gz" 2>/dev/null | cmp -s - "${gz%.gz}" || printf '%s\n' "$gz"
done)
[ -z "$STALE" ] ||
    die "a .gz in $UI_DIST does not hold the file beside it, and the appliance would send it in that file's place — rebuild the bundle, or remove the .gz: $STALE"

# No hooks: `appliance.toml` names no `hooks_dir`. The maintainer scripts run
# `run-parts` over /usr/share/$NAME/{postinst.d,prerm.d} when either exists, and
# naming a directory here is what ships something for them to run.

# No shipped files: `appliance.toml` has no `[package.files]` table.

# The journald drop-in `appliance-xtask` rendered, which postinst flushes into.
case "$TARGET" in
image)
    [ -f "$TARGET_SCAFFOLD/debian/journald.conf" ] ||
        die "$TARGET_SCAFFOLD/debian/journald.conf is not there — run \`appliance-xtask packaging render\`"
    ;;
esac

# The unit's real drop-ins, which ship; the `.sample` beside them does not. The
# name is held to a character set before the build, because a newline or a
# backslash in one is a name md5sums cannot carry — and a package-shipped
# drop-in has no reason to be called anything a keyboard would not type.
DROPINS="$SCAFFOLD/systemd/$NAME.service.d"
for conf in "$DROPINS"/*.conf; do
    [ -e "$conf" ] || continue
    case "${conf##*/}" in
    *[!A-Za-z0-9._-]*) die "$conf has a character in its name a shipped drop-in may not — letters, digits, \`.\`, \`_\` and \`-\`" ;;
    esac
    [ -f "$conf" ] || die "$conf is not a plain file, and a drop-in is one"
done

# The product's own DEP-5 copyright file, shipped to /usr/share/doc when it is
# there. A package without one installs; lintian objects, and a licence nobody
# can find is its own kind of problem.
[ -f "$SCAFFOLD/debian/copyright" ] ||
    warn "no $SCAFFOLD/debian/copyright — write a DEP-5 one and the next build ships it as /usr/share/doc/$NAME/copyright"

# The chassis maps the targets it has actually built for, each to its Debian
# architecture and to the ELF e_machine that proves the target took. Every one
# is musl: a package has to run on a box whose glibc nobody chose. Adding one
# is a change to appliance-xtask rather than an edit here, which every render
# would undo.
case "$CARGO_BUILD_TARGET" in
aarch64-*-musl) ARCH=arm64 MACHINE=183 ;;
x86_64-*-musl) ARCH=amd64 MACHINE=62 ;;
*) die "no Debian architecture is mapped for CARGO_BUILD_TARGET=$CARGO_BUILD_TARGET — the chassis maps aarch64-*-musl and x86_64-*-musl only" ;;
esac

# ---------------------------------------------------------------------------
# The build
# ---------------------------------------------------------------------------
# `--locked`, because a package is built from the lockfile a consumer committed
# rather than from whatever resolves on the day.
#
# **Cross-linking wants more than a cross compiler.** Beyond `CC_<triple>` and
# `AR_<triple>`, which only a dependency's build script reads, cargo takes the
# linker from `CARGO_TARGET_<TRIPLE>_LINKER`; without it the host's linker is
# used, and fails naming nothing. An aarch64 linker must apply
# `--fix-cortex-a53-843419`, which the check below holds it to: the chassis's
# BUILDING.md recipe uses `rust-lld` because `zig cc` refuses the flag.
cargo build --release --locked --target "$CARGO_BUILD_TARGET" --target-dir "$TARGET_DIR" --bin "$BINARY"
BIN="$TARGET_DIR/$CARGO_BUILD_TARGET/release/$BINARY"

# ---------------------------------------------------------------------------
# The four assertions this script exists to make
# ---------------------------------------------------------------------------
# Each is about a binary that compiled and linked and is still wrong, which is
# why none can be a compiler error. `appliance-build-id`'s module doc carries
# the reasoning for the third, and appliance.toml the history behind the first
# two.

# e_machine is two little-endian bytes at offset 18, read directly because
# `file`'s wording varies; a static binary never names a loader, and a musl
# build that was not static names one the box does not have.
GOT=$(od -An -tu2 -j18 -N2 "$BIN" | tr -d '[:space:]')
[ "$GOT" = "$MACHINE" ] ||
    die "$BIN has e_machine=$GOT, wanted $MACHINE for $CARGO_BUILD_TARGET — the toolchain did not build for the target it was told"
! grep -q -a -e ld-linux -e ld-musl "$BIN" ||
    die "$BIN names a dynamic loader — it is not the static binary a card needs"

# `tr -d` because some `wc` implementations pad the count, which lands in the
# message and, on a stricter `[`, in the comparison.
SIZE=$(wc -c < "$BIN" | tr -d '[:space:]')
[ "$SIZE" -ge "$MIN_BINARY_BYTES" ] ||
    die "$BIN is $SIZE bytes, under the $MIN_BINARY_BYTES floor — last time, the linker had dropped SQLite because nothing in the binary reached it"

# A build with no commit to name renders " (unknown)" into its own --version
# string, and shipping one gives a fleet two artefacts that each claim to be the
# other. The release profile strips the binary; this is a `&'static str` in
# .rodata rather than a symbol, so it survives that.
if grep -q -a ' (unknown)' "$BIN"; then
    die "$BIN cannot name the commit it was built from — build from a checkout, or set APPLIANCE_BUILD_ID with scaffold/packaging/build-id.sh"
fi

# A Cortex-A53 — the Pi 3, the Zero 2 W — can compute a wrong address at a site
# of erratum 843419 the linker left unpatched; how many there are moves with the
# layout, so a build with none proves nothing about the next.
if [ "$ARCH" = arm64 ]; then
    RC=0
    appliance-xtask erratum-843419 "$BIN" || RC=$?
    case "$RC" in
    0) ;;
    1) die "$BIN failed the Cortex-A53 erratum 843419 check" ;;
    *) die "could not run the Cortex-A53 erratum 843419 check (its error is above) — an older appliance-xtask cannot run it at all; install this chassis's tag's" ;;
    esac
fi

# ---------------------------------------------------------------------------
# The version
# ---------------------------------------------------------------------------
# **Asked for rather than assembled here.** The `~`, the date leading the sha and
# the `.dirty` spelling are three ordering rules, and they live in
# `appliance-build-id` so this script and the binary's own --version cannot drift.
#
# Every package this builds is a snapshot sorting *below* the release it names, so
# a real release installs over a box running one. Cutting the release version
# itself is not this script's job.
#
# A plain assignment from a command substitution, so `set -e` sees the tool's
# refusal — it exits non-zero when the build has no commit to make a version
# from, which includes a build id supplied through the environment.
BASE_VERSION=$(cargo pkgid -p "$PACKAGE" | sed -e 's|.*[#@]||')
VERSION=$(appliance-xtask build-id --deb-version "$BASE_VERSION")

# ---------------------------------------------------------------------------
# Staging
# ---------------------------------------------------------------------------
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

# **The staging root's own mode becomes the package's `./` entry, and dpkg
# applies that entry to `/`.** `mktemp -d` makes a 0700 directory, so without
# this the package asks dpkg to make the board's root directory unreadable to
# everyone but root — which is not a mistake anything downstream would catch.
#
# A debhelper-built package never meets this: its staging tree is made under the
# ordinary umask and is already 0755.
chmod 0755 "$STAGE"

install -D -m 0755 "$BIN" "$STAGE/usr/bin/$BINARY"

# `/usr/lib/systemd/system` rather than `/lib/systemd/system`: the release this
# chassis targets is usr-merged, and a package shipping into the aliased path is
# one dpkg has to reconcile against the symlink.
install -D -m 0644 "$UNIT" "$STAGE/usr/lib/systemd/system/$NAME.service"

# **The browser bundle, into the directory `appliance-config` defaults to.**
# Nothing on the box connects those two — the unit passes no arguments and no
# maintainer script writes a config.toml — so this path and that default are the
# whole mechanism, and a test holds them against each other rather than against
# this comment.
#
# **`install -d` and not `mkdir -p`**, which is the difference between an
# appliance that starts and one that does not. `install -d` sets the mode on
# every component it creates and ignores the umask; `mkdir -p` honours it. dpkg
# applies an archive directory's mode verbatim when it creates one, so a build
# host with `umask 077` shipped /usr/share/<name> at 0700 — and the daemon's own
# unprivileged user, being "other" against a root-owned directory, could not
# traverse it. Every other directory here comes from `install -D`, which never
# had this.
UI_STAGE="$STAGE/usr/share/$NAME/ui"
install -d -m 0755 "$UI_STAGE"

# The contents rather than the directory, so `dist/index.html` arrives at
# `ui/index.html` and not at `ui/dist/index.html`.
cp -R "$UI_DIST/." "$UI_STAGE"

# **The modes arriving here are the source tree's, not the package's.** `cp -R`
# preserves what the product's build and the build host's umask left behind — the
# same shape as `mktemp -d`'s 0700 becoming the staging root's mode above, an
# artefact inheriting a property of where it was assembled rather than of what
# was put in it. A group-writable file under /usr/share on an appliance is a mode
# nobody chose. A bundle is static files a daemon reads and nothing writes, so it
# is 0755 and 0644 flat.
find "$UI_STAGE" -type d -exec chmod 0755 {} +
find "$UI_STAGE" -type f -exec chmod 0644 {} +

# **This is where the mode that matters is set.** The mode inside the package is
# the only one that reaches the board — a mode in a consumer's working tree does
# not travel — and this one is load-bearing rather than tidy: dpkg-deb *refuses*
# a maintainer script outside 0555-0775 rather than fixing it, so getting this
# wrong is a failed build and never a bad package.
#
# It does not extend to `control` and `md5sums` below, whose modes dpkg-deb
# normalises to 0644 whatever the staging tree says. Setting those here would be
# a line that looks like this one and does nothing.
for script in postinst prerm postrm; do
    install -D -m 0755 "$TARGET_SCAFFOLD/debian/$script" "$STAGE/DEBIAN/$script"
done

# ---------------------------------------------------------------------------
# What the product ships beside the binary
# ---------------------------------------------------------------------------
# Every file under /etc is listed in DEBIAN/conffiles, so dpkg keeps an
# operator's edit across an upgrade and asks before replacing it. The list is
# appended to as each is staged, and it is fine for it not to exist afterwards:
# a package with no conffile has no file to say so.
CONFFILES="$STAGE/DEBIAN/conffiles"

# The unit's drop-ins, checked above, under the directory a package's belong
# in — `/etc/systemd/system` is the operator's. A product with none stages none.
for conf in "$DROPINS"/*.conf; do
    [ -e "$conf" ] || continue
    install -D -m 0644 "$conf" "$STAGE/usr/lib/systemd/system/$NAME.service.d/${conf##*/}"
done

# Nothing is staged under /usr/share/$NAME/postinst.d or prerm.d: no hooks_dir.

# Nothing is staged from [package.files].

# The journald drop-in, a vendor file numbered above the distribution's own.
case "$TARGET" in
image)
    install -D -m 0644 "$TARGET_SCAFFOLD/debian/journald.conf" "$STAGE/usr/lib/systemd/journald.conf.d/95-wiretap-appliance-persistent.conf"
    ;;
esac

# ---------------------------------------------------------------------------
# /usr/share/doc
# ---------------------------------------------------------------------------
# `copyright` is the product's own file, shipped as it is when it is there — a
# DEP-5 file names a copyright holder and a licence, which are not the chassis's
# to decide. The changelog is made here from the version this build is, because
# a hand-kept one and a snapshot version disagree from the second build on. It
# is `changelog.gz` rather than `changelog.Debian.gz`: a version with no Debian
# revision is a native package, and Policy 12.7 names a native one's that way.
DOC="$STAGE/usr/share/doc/$NAME"
install -d -m 0755 "$DOC"
if [ -f "$SCAFFOLD/debian/copyright" ]; then
    install -m 0644 "$SCAFFOLD/debian/copyright" "$DOC/copyright"
fi
# Two statements rather than a pipeline, so `set -e` sees either fail. The
# maintainer was read and checked before the build. `-n`, so the archive
# carries no name or timestamp of its own.
printf '%s (%s) unstable; urgency=medium\n\n  * Snapshot build %s.\n\n -- %s  %s\n' \
    "$NAME" "$VERSION" "$VERSION" "$MAINTAINER" "$(date -R)" > "$DOC/changelog"
gzip -9n "$DOC/changelog"
chmod 0644 "$DOC/changelog.gz"

# ---------------------------------------------------------------------------
# The control file
# ---------------------------------------------------------------------------
# `Installed-Size:` is an estimate, in KiB, of the space the installed package
# needs, and it does not count the control area. `dpkg-gencontrol` computes it
# for a package built the usual way; nothing in this pipeline is that, so it is
# computed here. Without it apt reports "0 B of additional disk space will be
# used" and its free-space check has nothing to work from — which on a
# card-sized filesystem is the check you want.
INSTALLED_SIZE=$(du -k -s --exclude=DEBIAN "$STAGE" | cut -f1)

# **`du` is not the last stage of that pipeline, so `set -e` cannot see it fail.**
# `--exclude` is GNU-only, so a BSD `du` exits 1, `cut` succeeds on nothing, the
# substitution fills the token with an empty string, the "nothing left unfilled"
# grep passes because the token *was* replaced, and dpkg-deb builds a package whose
# `Installed-Size:` is blank — the state the field exists to prevent.
case "$INSTALLED_SIZE" in
'' | *[!0-9]*)
    die "Installed-Size came out as '$INSTALLED_SIZE' — \`du -k -s --exclude=DEBIAN\` did not produce a number, and set -e cannot see a pipeline stage that is not the last"
    ;;
esac

sed -e "s|@VERSION@|$VERSION|g" \
    -e "s|@ARCH@|$ARCH|g" \
    -e "s|@INSTALLED_SIZE@|$INSTALLED_SIZE|g" \
    "$CONTROL_IN" > "$STAGE/DEBIAN/control"

# Nothing may be left unfilled, which is the rule appliance-xtask holds its own
# placeholders to, applied to the layer below it. dpkg-deb is not a backstop
# here: it refuses an unfilled `@VERSION@`, only *warns* about `@ARCH@` and
# builds anyway, and takes `@INSTALLED_SIZE@` as an ordinary field value in
# silence.
if grep -q -E '@[A-Z_]+@' "$STAGE/DEBIAN/control"; then
    die "$STAGE/DEBIAN/control still has an unfilled token in it"
fi

# What `dpkg --verify` reads. Paths are relative to the filesystem root with no
# leading `./`, and the control area is not listed.
#
# **NUL-separated on the way in**, because a browser bundle carries whatever a
# product's `public/` directory does: `Inter Regular.woff2` splits on the space and
# xargs reports a file called `Inter`, and `dont'panic.svg` is an unterminated
# quote. Both abort the build after the cross-build is already paid for.
#
# **Line-based on the way out.** dpkg's md5sums format is one record per line, so a
# name this pipeline could not carry has already been refused — which means NULs
# past this point buy nothing.
#
# The `./` comes off md5sum's *output* rather than the paths going in, so the match
# is anchored on the two-space separator that format uses rather than on the start
# of the line, where the digest is. Hex cannot contain `/`, so the leftmost `  ./`
# is always the separator, including on the `\`-prefixed lines md5sum writes for an
# escaped name.
(
    cd "$STAGE" && find . -type f ! -path './DEBIAN/*' -print0 |
        LC_ALL=C sort -z | xargs -0 md5sum | sed 's|  \./|  |' > DEBIAN/md5sums
)

# **Every stage of a pipeline but the last is invisible to `set -e`**, and that one
# has four. Counting what came out against what was staged is the only thing here
# that catches a stage that failed silently.
#
# `wc -l` counts lines rather than files, which is right *because* a name with a
# newline was refused before the build. Those two facts are one decision, and
# moving either alone makes this count lie.
STAGED=$(cd "$STAGE" && find . -type f ! -path './DEBIAN/*' | wc -l | tr -d '[:space:]')
LISTED=$(wc -l < "$STAGE/DEBIAN/md5sums" | tr -d '[:space:]')
[ "$STAGED" -eq "$LISTED" ] ||
    die "DEBIAN/md5sums lists $LISTED of $STAGED staged files — either a stage of the pipeline that writes it failed, which set -e cannot see, or a staged name holds a newline and this format cannot carry it"

# ---------------------------------------------------------------------------
# The package
# ---------------------------------------------------------------------------
mkdir -p "$OUT"
DEB="$OUT/${NAME}_${VERSION}_${ARCH}.deb"

# **`--root-owner-group`**, or every path in the package is owned by whichever
# uid the build host happened to run as — which arrives on the board as a
# numeric owner that means something there and nothing here.
dpkg-deb --root-owner-group --build "$STAGE" "$DEB"

echo "built $DEB"
