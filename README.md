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
reference the port is tested against, and it still runs.

**Nothing is packaged yet.** The Debian packages — `wiretap-server` and `wiretap-web` as
static musl binaries for amd64 and arm64, installable on any distribution with systemd —
arrive with the first tagged release, along with a multi-architecture gateway image.

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
| [debian/](debian/) | Package metadata |

## Build

```sh
cargo test --workspace
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

## Licence

MIT — see [LICENSE](LICENSE).
