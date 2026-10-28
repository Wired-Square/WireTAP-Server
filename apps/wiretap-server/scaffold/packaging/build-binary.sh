#!/bin/sh
# Cross-builds wiretap-server-ui's binary and refuses one no artefact should carry, for
# make-deb.sh and make-image.sh. They source it, having set BINARY, TARGET_DIR,
# CARGO_BUILD_TARGET, `die` and, when it is not BINARY, BINARY_PACKAGE; it
# defines two functions and runs nothing.
#
# Owned by wslib-appliance-xtask, and rewritten unconditionally by every render.

MIN_BINARY_BYTES='9000000'

# wslib-appliance-xtask maps the targets it has actually built for, each to
# its architecture — Debian's name and Docker's, which agree — and to the ELF
# e_machine that proves the target took. Every one is musl: a package or image
# has to run on a box whose glibc nobody chose. Adding one is a change to
# wslib-appliance-xtask rather than an edit here, which every render would undo.
binary_arch() {
    case "$CARGO_BUILD_TARGET" in
    aarch64-*-musl) ARCH=arm64 MACHINE=183 ;;
    x86_64-*-musl) ARCH=amd64 MACHINE=62 ;;
    *) die "no architecture is mapped for CARGO_BUILD_TARGET=$CARGO_BUILD_TARGET — wslib-appliance-xtask maps aarch64-*-musl and x86_64-*-musl only" ;;
    esac
}

# Builds the binary into BIN, after binary_arch.
build_binary() {
    # `--locked`, because a package or image is built from the lockfile a
    # consumer committed rather than from whatever resolves on the day.
    #
    # **Cross-linking wants more than a cross compiler.** Beyond `CC_<triple>` and
    # `AR_<triple>`, which only a dependency's build script reads, cargo takes the
    # linker from `CARGO_TARGET_<TRIPLE>_LINKER`; without it the host's linker is
    # used, and fails naming nothing. An aarch64 linker must apply
    # `--fix-cortex-a53-843419`, which the check below holds it to: use
    # `rust-lld`, since `zig cc` refuses the flag (wslib-appliance-xtask's
    # README, "Cross-building").
    cargo build --release --locked --target "$CARGO_BUILD_TARGET" --target-dir "$TARGET_DIR" -p "${BINARY_PACKAGE:-$BINARY}" --bin "$BINARY"
    BIN="$TARGET_DIR/$CARGO_BUILD_TARGET/release/$BINARY"

    # The four assertions this file exists to make. Each is about a binary that
    # compiled and linked and is still wrong, which is why none can be a compiler
    # error. `wslib-build-id`'s module doc carries the reasoning for the
    # third, and appliance.toml the history behind the first two.

    # e_machine is two little-endian bytes at offset 18, read directly because
    # `file`'s wording varies; a static binary never names a loader, and a musl
    # build that was not static names one the box does not have.
    GOT=$(od -An -tu2 -j18 -N2 "$BIN" | tr -d '[:space:]')
    [ "$GOT" = "$MACHINE" ] ||
        die "$BIN has e_machine=$GOT, wanted $MACHINE for $CARGO_BUILD_TARGET — the toolchain did not build for the target it was told"
    ! grep -q -a -e ld-linux -e ld-musl "$BIN" ||
        die "$BIN names a dynamic loader — it is not the static binary a package or image needs"

    # `tr -d` because some `wc` implementations pad the count, which lands in the
    # message and, on a stricter `[`, in the comparison.
    SIZE=$(wc -c < "$BIN" | tr -d '[:space:]')
    [ "$SIZE" -ge "$MIN_BINARY_BYTES" ] ||
        die "$BIN is $SIZE bytes, under the $MIN_BINARY_BYTES floor — last time, the linker had dropped SQLite because nothing in the binary reached it"

    # A build with no commit to name renders " (unknown)" into its own --version
    # string, and shipping one gives a fleet two artefacts that each claim to be
    # the other. The release profile strips the binary; this is a `&'static str`
    # in .rodata rather than a symbol, so it survives that.
    if grep -q -a ' (unknown)' "$BIN"; then
        die "$BIN cannot name the commit it was built from — build from a checkout, or set WSLIB_BUILD_ID with scaffold/packaging/build-id.sh"
    fi

    # A Cortex-A53 — the Pi 3, the Zero 2 W — can compute a wrong address at a
    # site of erratum 843419 the linker left unpatched; how many there are moves
    # with the layout, so a build with none proves nothing about the next.
    if [ "$ARCH" = arm64 ]; then
        RC=0
        wslib-appliance-xtask erratum-843419 "$BIN" || RC=$?
        case "$RC" in
        0) ;;
        1) die "$BIN failed the Cortex-A53 erratum 843419 check" ;;
        *) die "could not run the Cortex-A53 erratum 843419 check (its error is above) — cargo install wslib-appliance-xtask --registry wired-square" ;;
        esac
    fi
}
