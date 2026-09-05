# WireTAP Server

The capture server and database gateway for [WireTAP](https://github.com/Wired-Square/WireTAP).
Two programs that run on your network rather than on your desktop:

- **`wiretap-server`** — captures CAN frames from a Linux host's SocketCAN interfaces,
  bridges them live to the WireTAP desktop app and SavvyCAN over the GVRET protocol, and
  forwards them to a gateway for archiving. Accepts pushed frames from microcontroller
  capture devices over a binary TCP protocol. Caches to local disk through a gateway outage
  and drains in order when it returns.
- **`wiretap-backend`** — the gateway. Owns a TimescaleDB, and is the only thing that talks
  to it: not the desktop app, not capture devices, not the server. Serves an HTTP query API,
  a browser admin interface, and the binary ingest listener.

A third package, `wiretap-web`, will add a browser interface for administering a capture
appliance.

## Status

**Early.** The gateway is production code and moved here unchanged — run it with Docker as
described below. The capture server is being ported from the Python implementation that
lived in the WireTAP repo; that Python is kept at [tools/oracle/](tools/oracle/) as the
reference the port is tested against, and it still runs. The Rust now does all of it —
capture, the GVRET bridge, archiving to a gateway, the disk cache that carries an outage,
and the listener for pushed frames — but it has not been through the field validation that
earns the switch, so the Python is still what a deployment should run.

**The capture server is packaged and the package installs; nothing is published yet.**
`packaging/make-deb.sh --arch all` builds `wiretap-server` as a static musl `.deb` for
arm64 and amd64, installable on any distribution with systemd. The arm64 package has been
taken through its whole lifecycle on Debian bookworm — install, reinstall, an upgrade from
an existing Python deployment, remove, purge, and an image-build chroot with no PID 1 — and
the disk cache was counted at every step. Nothing is published yet, but the workflow that
would is written: a `v*` tag drafts a release, attaches both `.deb` files with
`SHA256SUMS`, and pushes a multi-architecture gateway image to GHCR for a human to publish.
The `wiretap-web` package arrives later.

## Running the gateway

```bash
cd crates/wiretap-backend
```

then follow [its README](crates/wiretap-backend/README.md#quick-start). For a production
host that cannot build images, see [crates/wiretap-backend/deploy/](crates/wiretap-backend/deploy/).

## Layout

| Path | What |
| --- | --- |
| [crates/wiretap-backend/](crates/wiretap-backend/) | The gateway: HTTP API, ingest listener, admin SPA, Docker stack, capture schema |
| [tools/](tools/) | Test and admin scripts. Never packaged, never shipped |
| [tools/oracle/](tools/oracle/) | The Python server, kept runnable as the port's reference |
| [docs/ingest-protocol.md](docs/ingest-protocol.md) | The binary ingest wire format, for anyone writing capture-device firmware |
| [debian/](debian/) | Package metadata, and the maintainer scripts |
| [packaging/](packaging/) | `make-deb.sh`, the systemd unit, the reference config, and the lifecycle test |

## Build

The four gates, in the order [CI](.github/workflows/ci.yml) runs them:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p wiretap-server --target aarch64-unknown-linux-musl --all-targets --locked -- -D warnings
cargo test --workspace
```

The third is not redundant. The SocketCAN capture modules are
`#[cfg(target_os = "linux")]`, so on a macOS host nothing else lints them at all. It needs
`rustup target add aarch64-unknown-linux-musl` and `zig` on `PATH` — bundled SQLite means a
build script compiles C, and `.cargo/config.toml` points that at
[packaging/zigcc](packaging/zigcc).

Capturing from a CAN interface can only be *run* on Linux, so those tests are `#[ignore]`d
and CI executes them against a virtual bus. On any Linux host:

```sh
sudo modprobe vcan && sudo ip link add dev vcan0 type vcan && sudo ip link set up vcan0
cargo test -p wiretap-server --test vcan_loopback -- --ignored --test-threads=1
```

## Packaging

```sh
packaging/make-deb.sh --arch all      # target/deb/wiretap-server_<version>_<arch>.deb
```

Needs `cargo-zigbuild`, `zig` and `dpkg-deb`, and runs on macOS as well as Linux — nothing
is compiled by the packaging itself.

The script refuses to build a package that could not work. Some of that is about the
binary: the ELF architecture, that the musl target actually took, and a size floor that
catches SQLite being dead-code-eliminated. The rest is about the several files that have to
agree with each other and that nothing at runtime notices have stopped agreeing — the
unit's `ExecStart` and its `-C` flag, the address families the CAN socket and the bitrate
query need, the paths and the system user shared between `debian/postinst` and
`debian/postrm`, and the names the packaging borrows from the daemon rather than owning
(the staged-cache filenames, and the `STATE_DIRECTORY` the documented `--check-config`
command is prefixed with). Each is compared against whichever file actually owns it, so a
mismatch is a failed build rather than a host that looks healthy and archives nothing.

### Testing the package

[packaging/tests/deb-lifecycle.sh](packaging/tests/deb-lifecycle.sh) takes a built `.deb`
through install, reinstall over both a stopped and a running daemon, a disk cache it cannot
write, remove and purge — nine steps, asserting at each, and counting the cache throughout.
CI runs it on the runner, which is itself a booted systemd host.

It installs and purges a real system package, so run it somewhere disposable; `--yes` is
the acknowledgement of that. The maintainer scripts do most of their work only when systemd
is running, so locally that means a container which boots it — no Raspberry Pi needed:

```sh
packaging/make-deb.sh --arch arm64
docker run -d --name wt --privileged --cgroupns=host \
  -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock \
  -v "$PWD:/src:ro" <debian-bookworm-with-systemd> /sbin/init
docker exec wt sh -c 'cp -r /src /work'
docker exec wt /work/packaging/tests/deb-lifecycle.sh --yes \
  /work/target/deb/wiretap-server_0.1.0_arm64.deb
```

The image is stock `debian:bookworm` plus `systemd systemd-sysv dbus init-system-helpers
adduser sqlite3 python3`. Rebuild the `.deb` first — `target/deb/` is a build artefact and
says nothing about what is committed.

## Licence

MIT — see [LICENSE](LICENSE).
