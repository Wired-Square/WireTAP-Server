# WireTAP Server

The capture server and database gateway for
[WireTAP](https://github.com/Wired-Square/WireTAP). Two programs that run on your
network rather than on your desktop:

- **`wiretap-server`** — captures CAN frames from a Linux host's SocketCAN
  interfaces, bridges them live to the WireTAP desktop and SavvyCAN over the
  GVRET protocol, and forwards them to a gateway for archiving. Accepts frames
  pushed by microcontroller capture devices over a binary TCP protocol, and
  caches to local disk through a gateway outage.
- **`wiretap-backend`** — the gateway. Owns a TimescaleDB and is the only thing
  that talks to it: not the desktop, not capture devices, not the server. Serves
  an HTTP query API, a browser admin interface, and the ingest listener.

It is a **read-only tap**. The one exception is the Test Pattern responder, which
transmits and is disabled unless explicitly armed.

## Running the gateway

```sh
cd crates/wiretap-backend
```

then follow [its README](crates/wiretap-backend/README.md#quick-start). For a
production host that cannot build images, see
[crates/wiretap-backend/deploy/](crates/wiretap-backend/deploy/).

## Running the capture server

Capture two buses, bridge them on the GVRET port, and archive to a gateway:

```sh
wiretap-server --iface can0,can1 \
               --forward-enable --forward-host gateway.local \
               --forward-api-key "$WIRETAP_FORWARD_TOKEN"
```

Or put it in a file — `packaging/wiretap-server.toml` is the annotated reference,
installed to `/etc/wiretap-server/` by the package:

```toml
[server]
iface = "can0,can1"

[forward]
enable = true
host = "gateway.local"
```

```sh
wiretap-server -C /etc/wiretap-server/wiretap-server.toml
```

**Values in the config file override command-line flags**, not the other way
round. That is deliberate — deployed units pass both — and
`wiretap-server --check-config` prints what a given combination actually resolves
to, so it never has to be argued about.

`--iface ""` is an ingest-only deployment: no local CAN hardware, just the
listener and the archive. `--help` lists the rest; the ones worth knowing are
`--can-fd`, `--ingest-enable`, `--echo-console` and `--test-pattern-enable`.

Anything `iface` cannot say is a `[[device]]` table — a serial line tapped for
Modbus RTU, a CAN bus that must never be transmitted on, a device whose frames
belong in a different gateway database:

```toml
[[device]]
kind = "serial"
interface = "/dev/ttyUSB0"
baud = 9600
framing = "modbus-rtu"
database = "sungrow_rs485"
```

A serial device is opened read-only and every function code is framed, so a
line full of a vendor's own codes is captured without knowing them first. The
shipped config documents every key, and `--check-config` lists each device with
its bus number and database.

Port 23 is the GVRET default and needs `CAP_NET_BIND_SERVICE`; the packaged unit
grants it.

## Layout

| Path | What |
| --- | --- |
| [crates/wiretap-backend/](crates/wiretap-backend/) | The gateway: HTTP API, ingest listener, admin SPA, Docker stack, capture schema |
| [docs/ingest-protocol.md](docs/ingest-protocol.md) | The binary ingest wire format, for anyone writing capture-device firmware |
| [tools/](tools/) | Test and admin scripts. Never packaged, never shipped |
| [debian/](debian/), [packaging/](packaging/) | Package metadata, maintainer scripts, `make-deb.sh`, the systemd unit and the lifecycle test |

## Build

The four gates, in the order [CI](.github/workflows/ci.yml) runs them:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p wiretap-server --target aarch64-unknown-linux-musl --all-targets --locked -- -D warnings
cargo test --workspace
```

The third is not redundant: the SocketCAN modules are
`#[cfg(target_os = "linux")]`, so on a macOS host nothing else lints them at all.
It needs `rustup target add aarch64-unknown-linux-musl` and `zig` on `PATH` —
bundled SQLite means a build script compiles C, and `.cargo/config.toml` points
that at [packaging/zigcc](packaging/zigcc).

Capture can only be *run* on Linux, so those tests are `#[ignore]`d and CI runs
them against a virtual bus. On any Linux host:

```sh
sudo modprobe vcan && sudo ip link add dev vcan0 type vcan && sudo ip link set up vcan0
cargo test -p wiretap-server --test vcan_loopback -- --ignored --test-threads=1
```

## Packaging

```sh
packaging/make-deb.sh --arch all    # target/deb/wiretap-server_<version>_<arch>.deb
```

A static musl `.deb` for arm64 and amd64, installable on any distribution with
systemd. Needs `cargo-zigbuild`, `zig` and `dpkg-deb`; runs on macOS as well as
Linux. The script refuses to build a package that could not work — it checks the
ELF architecture, that the musl target took, and that the several files which
must agree with one another still do.

[packaging/tests/deb-lifecycle.sh](packaging/tests/deb-lifecycle.sh) takes a
built `.deb` through install, reinstall, remove, purge and a chroot — nine steps,
asserting at each. It installs and purges a real system package, so run it
somewhere disposable; `--yes` is the acknowledgement of that.

### Which build is this?

Every artefact names the commit it was built from, because a version alone cannot
tell two builds apart — every `0.1.0` looks identical.

```sh
wiretap-server --version                          # wiretap-server 0.1.0 (g0123456789ab)
dpkg-query -W wiretap-server                      # 0.1.0~git20260910.0123456789ab
curl -s localhost:8423/v1/health | jq .version    # the gateway
```

The daemon logs the same string as its first journal line, so a running capture
can be identified without stopping it. Package versions sort *below* the release
they precede — `0.1.0~git… < 0.1.0~rc1 < 0.1.0` — so a release always installs
cleanly over a test build.

## Licence

MIT — see [LICENSE](LICENSE).
