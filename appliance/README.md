# WireTAP appliance

The Raspberry Pi appliance: `wiretap-appliance`, a web daemon that administers
the box from a browser, beside `wiretap-server`, which captures as it does on
any Debian host. It is built on the Wired Square appliance chassis
([`wslib-appliance-rs`](https://github.com/Wired-Square/wslib-appliance-rs),
pinned at `v0.1.1`), which supplies HTTPS, accounts, the host, network, SSH,
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
| `scripts/stage-wiretap-server.sh` | puts `wiretap-server`'s package into the image |

## Changing the scaffold

The chassis owns most of `scaffold/` and rewrites it on every
`appliance-xtask packaging render`; this product's own files there are
`debian/copyright`, the unit drop-ins in
`systemd/wiretap-appliance.service.d/`, and the image's substages and checks
numbered 10 to 89. Change `appliance.toml` and render
again; CI fails on a hand-edit to a chassis-owned file. Install the tool at
the lockfile's chassis:

```sh
rev="$(grep -om1 'wslib-appliance-rs\.git?[^#"]*#[0-9a-f]*' Cargo.lock | cut -d'#' -f2)"
cargo install --locked --git https://github.com/Wired-Square/wslib-appliance-rs.git --rev "$rev" appliance-xtask
```

## Running it on a development machine

The card's unit switches on host administration and the browser's
first-account screen; a plain `cargo run` has both off. From this directory:

```sh
WIRETAP_APPLIANCE_PRIVILEGE=host-admin WIRETAP_APPLIANCE_ONBOARDING=browser \
  cargo run -- --bind-address 127.0.0.1 --redirect-port 0 --socket /tmp/wiretap-appliance.sock
```

It serves `https://127.0.0.1:8443` and keeps its database and certificate in
the working directory. With no system D-Bus, the host screens say host
administration is unavailable.

## Building the package

From this directory, after `npm ci && npm run build` in `frontend/`:

```sh
scaffold/packaging/make-deb.sh --target image    # -> target/debian/image/wiretap-appliance_*_arm64.deb
```

It cross-builds a static `aarch64-unknown-linux-musl` binary and wants `cargo`,
`dpkg-deb`, `appliance-xtask`, zig and the cross-link variables in the
chassis's `BUILDING.md`. On a Mac, [docs/workspaces.md](../docs/workspaces.md#cross-building-the-appliance-on-a-mac)
has the whole environment.

## Building the image

pi-gen needs Debian and root, so the image is built on a Debian build host.
Copy `scaffold/image/config.local.example` to `scaffold/image/config.local`,
which is git-ignored, and set your SSH public key and `BUILD_HOST` there. A
`TAILSCALE_AUTH_KEY` there makes every card join the tailnet; without one, a
card joins only when enrolled from the Network screen. Then:

```sh
(cd .. && packaging/make-deb.sh --arch arm64)   # wiretap-server, into ../target/deb/
scripts/stage-wiretap-server.sh                 # copies it into the image's 10-wiretap substage
scaffold/packaging/make-deb.sh --target image   # wiretap-appliance, as above
scaffold/image/build-remote.sh                  # builds there, fetches the .img.xz into scaffold/image/deploy/
```

Write the `.img.xz` with Raspberry Pi Imager's **Use custom**, declining its OS
customisation.

The staging script refuses to pick between two `wiretap-server` packages in
`../target/deb/`; delete the one the image should not carry.

`scaffold/image/tests/run-verify-tests.sh` proves the image's build-time checks
in seconds, with no build.

## Enabling the local gateway

The image carries a WireTAP gateway, `wiretap-backend` and TimescaleDB in
Docker, installed and off: `wiretap-gateway.service` runs
`/usr/share/wiretap-appliance/gateway/compose.yaml`. Its images are pulled on
its first start, so that start needs the internet and takes a while.

The database must not live on the SD card. The unit refuses to start, and says
so, until storage is mounted at `/srv/wiretap-gateway`. On the card:

1. Mount a USB or NVMe drive there for good: an `/etc/fstab` line such as
   `UUID=<the drive's> /srv/wiretap-gateway ext4 defaults,nofail 0 2`, then
   `mount /srv/wiretap-gateway`.
2. Copy `/usr/share/wiretap-appliance/gateway/gateway.env.example` to
   `/etc/wiretap-gateway/gateway.env`, mode 0600, and set `POSTGRES_PASSWORD`
   and `WIRETAP_ADMIN_KEY` (`openssl rand -hex 32` for each).
3. `systemctl enable --now wiretap-gateway`.

The admin UI is then at `http://<card>:8423/admin`, signed in with the admin
key. Make an `ingest` key there and point `wiretap-server`'s `[forward]` at
`127.0.0.1:9323` with it.

dockerd is not started at boot. The gateway's unit starts it through its
socket, so a card with the gateway off does not run it.

**What the SD card is written with.** `wiretap-server`'s disk cache fills only
while its gateway is unreachable, and drains when it comes back. The gateway's
database is on the external drive. The SD card takes the container images once
per upgrade, under `/var/lib/docker`, and the containers' logs through the
journal, within journald's own limits.
