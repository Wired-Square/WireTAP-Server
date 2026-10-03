# WireTAP appliance

The Raspberry Pi appliance: `wiretap-appliance`, a web daemon that administers
the box from a browser, beside `wiretap-server`, which captures as it does on
any Debian host. It is built on the Wired Square appliance chassis
([`appliance-rpi-bootstrap`](https://github.com/Wired-Square/appliance-rpi-bootstrap),
pinned at `v0.15.6`), which supplies HTTPS, accounts, the host, network, SSH,
certificate, backup and log screens, the package and the card image.

This is a cargo workspace of its own, not a member of the root one, because
**the chassis is a private repository**. The root workspace and its release
build without it. Building anything here needs read access to the chassis,
and so does this directory's CI: on a fork, the three `appliance` jobs fail
fetching it, and the rest of CI is unaffected.

| Path | What |
| --- | --- |
| `crates/wiretap-appliance` | the daemon |
| `frontend` | its browser half: the chassis's shell and Settings screens |
| `appliance.toml` | the system identity the scaffold is rendered from |
| `scaffold/` | the package and image scripts, mostly chassis-owned |

## Changing the scaffold

The chassis owns most of `scaffold/` and rewrites it on every
`appliance-xtask packaging render`; this product's own files there are
`debian/copyright` and the unit drop-ins in
`systemd/wiretap-appliance.service.d/`. Change `appliance.toml` and render
again; CI fails on a hand-edit to a chassis-owned file. Install the tool at
the lockfile's chassis:

```sh
rev="$(grep -om1 'appliance-rpi-bootstrap\.git?[^#"]*#[0-9a-f]*' Cargo.lock | cut -d'#' -f2)"
cargo install --locked --git https://github.com/Wired-Square/appliance-rpi-bootstrap.git --rev "$rev" appliance-xtask
```

## Building the package

From this directory, after `npm ci && npm run build` in `frontend/`:

```sh
scaffold/packaging/make-deb.sh    # -> target/debian/wiretap-appliance_*_arm64.deb
```

It cross-builds a static `aarch64-unknown-linux-musl` binary and wants `cargo`,
`dpkg-deb`, `appliance-xtask`, zig and the cross-link variables in the
chassis's `BUILDING.md`. On a Mac, put GNU coreutils first on `PATH`.

## Building the image

pi-gen needs Debian and root, so the image is built on a Debian build host.
Copy `scaffold/image/config.local.example` to `scaffold/image/config.local`,
which is git-ignored, and set your SSH public key and `BUILD_HOST` there. Then,
with the package built:

```sh
scaffold/image/build-remote.sh    # builds there, fetches the .img.xz back
```

`scaffold/image/tests/run-verify-tests.sh` proves the image's build-time checks
in seconds, with no build.
