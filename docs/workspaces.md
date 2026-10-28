# The two cargo workspaces

The root workspace is `wiretap-server` and `wiretap-backend`.
`apps/wiretap-server/` is a second one, holding `wiretap-server-ui`, the capture
daemon's web UI. It is not a member of the root.

## Why two

`wiretap-server-ui` is built on the Wired Square appliance chassis,
[`wslib-appliance-rs`](https://github.com/Wired-Square/wslib-appliance-rs), which
is a private repository. Were it a member, the root's lockfile would name the
chassis and every build of it would need read access. Kept apart, the root and
its release build without the key, though not without the registry below. Each
workspace has its own `Cargo.lock` and its own `rust-version`: 1.88 in both.
`apps/wiretap-server/` pins chassis v0.17.1.

## The private registry

The root and `apps/wiretap-server/` take the `wslib-*` crates from the
`wired-square` Cargo registry on Forgejo, which `.cargo/config.toml` declares,
so a fork cannot build either. Cargo reads its read token from
`CARGO_REGISTRIES_WIRED_SQUARE_TOKEN`, as `Bearer <token>`, or from
`~/.cargo/credentials.toml`.

Off the network, Forgejo is out of reach and a read-only mirror stands in:
[`.cargo/mirror.toml`](../.cargo/mirror.toml) in `$CARGO_HOME/config.toml`
replaces the Forgejo source with it, and its token is
`CARGO_REGISTRIES_WIRED_SQUARE_MIRROR_TOKEN`. `Cargo.lock` records Forgejo
either way. The gateway image always reads the mirror, with that token as the
BuildKit secret `cargo-mirror-token`;
[`scripts/cargo-mirror-token.sh`](../scripts/cargo-mirror-token.sh) finds it.

The server app's browser half takes `@wired-square/kit` from the mirror's npm
half, which `apps/wiretap-server/frontend/.npmrc` declares; the same token goes
in your own `~/.npmrc` as `//mirror.wiredsquare.com/npm/:_authToken=<token>`.

The root's `.cargo/config.toml` still applies under `apps/wiretap-server/`,
since cargo reads config from parent directories, so a
cargo command there gets the root's musl `CC`.

## Building and gating each

The root's four gates are in the [README](../README.md#build): `fmt`, `clippy`,
`clippy` for `aarch64-unknown-linux-musl`, and `test`. CI runs them in its
`gates` job, beside the vcan capture drill, the package lifecycle and the
gateway image.

`apps/wiretap-server/` has three jobs of its own in
[CI](../.github/workflows/ci.yml), each fetching the chassis with a deploy key,
so on a fork they fail and nothing else does:

| Job | What it runs, in `apps/wiretap-server/` |
| --- | --- |
| Server UI gates | the same four gates, with the aarch64 `clippy` over the whole workspace |
| Server UI browser half | `npm ci`, then `npm run ci` (typecheck, unit tests and build) in `frontend/` |
| Server app image | `make-image.sh amd64`, keeping nothing |

Locally, `CARGO_NET_GIT_FETCH_WITH_CLI=true` has cargo fetch the private
chassis with your own git and its credentials.

## Cross-building the UI's package on a Mac

`clippy` stops before linking, so the root's config is enough for it. The
package, `apps/wiretap-server/scaffold/packaging/make-deb.sh --target deb`,
links a real `aarch64-unknown-linux-musl` binary, and on a Mac that needs all
of this:

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
line, in `apps/wiretap-server/frontend/`:

```sh
npm install "@wired-square/appliance-ui@github:Wired-Square/wslib-appliance-rs#vX.Y.Z"
```

A plain `npm install` after editing `package.json` leaves `package-lock.json`
on the old commit.
