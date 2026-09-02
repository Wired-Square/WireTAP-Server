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

A third package, `wiretap-web`, adds a browser interface for administering a capture
appliance. It is not built yet.

## Status

**Early.** The server is being ported from the Python implementation that lived in the
WireTAP repo; that Python is kept at [tools/oracle/](tools/oracle/) as the reference the
port is tested against, and it still runs. The gateway is production code and moved here
unchanged.

## Install

### Debian and Raspberry Pi OS

Download the `.deb` for your architecture from the
[latest release](https://github.com/Wired-Square/WireTAP-Server/releases) and install it:

```sh
sudo apt install ./wiretap-server_0.1.0_arm64.deb   # or _amd64.deb
sudoedit /etc/wiretap-server/wiretap-server.toml    # set iface and [forward]
sudo systemctl enable --now wiretap-server
```

The binaries are statically linked against musl, so the packages declare no library
dependencies and install on any distribution with systemd — a Raspberry Pi on arm64, or a
VM or container on amd64.

### The gateway, with Docker

```sh
cd crates/wiretap-backend
cp .env.example .env          # set POSTGRES_PASSWORD
docker compose up -d --build
curl http://localhost:8423/v1/health
open http://localhost:8423/admin
```

For a production host that cannot build images, see
[crates/wiretap-backend/deploy/](crates/wiretap-backend/deploy/).

## Layout

| Path | What |
| --- | --- |
| [crates/wiretap-backend/](crates/wiretap-backend/) | The gateway: HTTP API, ingest listener, admin SPA, Docker stack, capture schema |
| [tools/](tools/) | Test and admin scripts. Never packaged, never shipped |
| [tools/oracle/](tools/oracle/) | The Python server, kept runnable as the port's reference |
| [docs/](docs/) | The ingest and GVRET protocol specifications |
| [debian/](debian/), [packaging/](packaging/) | Package metadata, build scripts, appliance image |

## Build

```sh
cargo test --workspace
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

## Licence

MIT — see [LICENSE](LICENSE).
