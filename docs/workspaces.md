# The two cargo workspaces

The root workspace is `wiretap-server` and `wiretap-backend`. `appliance/` is a
second one, holding `wiretap-appliance` and the card image's scaffold, and it is
not a member of the root.

## Why two

`wiretap-appliance` is built on the Wired Square appliance chassis,
[`wslib-appliance-rs`](https://github.com/Wired-Square/wslib-appliance-rs), which
is a private repository. Were `appliance/` a member, the root's lockfile would
name the chassis and every build of it would need read access. Kept apart, the
public root, its release and any fork build without the key. Each workspace has
its own `Cargo.lock` and its own `rust-version`: 1.85 at the root, 1.88 in
`appliance/`.

The root's `.cargo/config.toml` still applies under `appliance/`, since cargo
reads config from parent directories, so a cargo command there gets the root's
musl `CC`.

## Building and gating each

The root's four gates are in the [README](../README.md#build): `fmt`, `clippy`,
`clippy` for `aarch64-unknown-linux-musl`, and `test`. CI runs them in its
`gates` job, beside the vcan capture drill, the package lifecycle and the
gateway image.

`appliance/` has three jobs of its own in [CI](../.github/workflows/ci.yml),
each fetching the chassis with a deploy key, so on a fork they fail and nothing
else does:

| Job | What it runs, in `appliance/` |
| --- | --- |
| Appliance gates | the same four gates, with the aarch64 `clippy` over the whole workspace |
| Appliance browser half | `npm ci`, then `npm run ci` (typecheck and build) in `frontend/` |
| Appliance packaging | `appliance-xtask packaging diff` (the scaffold is what the chassis renders), then `scaffold/image/tests/run-verify-tests.sh` |

Locally, `CARGO_NET_GIT_FETCH_WITH_CLI=true` has cargo fetch the private
chassis with your own git and its credentials.

## Cross-building the appliance on a Mac

`clippy` stops before linking, so the root's config is enough for it. The
package, `appliance/scaffold/packaging/make-deb.sh --target image`, links a real
`aarch64-unknown-linux-musl` binary, and on a Mac that needs all of this:

```sh
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export PATH="$(brew --prefix coreutils)/libexec/gnubin:$PATH"
export CC_aarch64_unknown_linux_musl=~/.cross/cc
export AR_aarch64_unknown_linux_musl=~/.cross/ar
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/rust-lld"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C link-arg=--fix-cortex-a53-843419"
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
```

- `~/.cross/cc` and `~/.cross/ar` are the zig shims the chassis's `BUILDING.md`
  makes.
- **The linker is rustc's own `rust-lld`.** Left unset, cargo links with the
  host `cc`, and Apple's `ld` rejects the GNU flags. With `zig cc` as the linker
  the link fails on a duplicate `_start`, and zig refuses the erratum flag.
- **No `RUSTFLAGS`**, not even an empty one. Any value replaces the target's
  `…_RUSTFLAGS`, the link succeeds without the Cortex-A53 fix, and `make-deb.sh`
  refuses the binary.
- GNU coreutils go first on `PATH` because `make-deb.sh` uses `install -D`,
  `du --exclude` and `md5sum`. It also wants Homebrew's `dpkg` and
  `appliance-xtask`.

**Moving the frontend to a new chassis tag** needs the tag named on the command
line, in `appliance/frontend/`:

```sh
npm install "@wired-square/appliance-ui@github:Wired-Square/wslib-appliance-rs#vX.Y.Z"
```

A plain `npm install` after editing `package.json` leaves `package-lock.json`
on the old commit.

## Building the image

[appliance/README.md](../appliance/README.md#building-the-image) has the steps.
The order is `packaging/make-deb.sh --arch arm64` at the root, then
`appliance/scripts/stage-wiretap-server.sh`, then
`appliance/scaffold/packaging/make-deb.sh --target image`, then
`appliance/scaffold/image/build-remote.sh`. The build box and a root SSH key
come from `appliance/scaffold/image/config.local`, which is git-ignored. Without
a `TAILSCALE_AUTH_KEY` there, a card has Tailscale but does not join the
tailnet until it is enrolled from the Network screen.
