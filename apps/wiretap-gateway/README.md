# wiretap-gateway, the ApplianceOS app

One image holding `wiretap-backend`, its own PostgreSQL 16 with TimescaleDB
2.29.2, the capture daemon `wiretap-server`, which archives the box's own
CAN buses into it, and the web UI `wiretap-server-ui`, which serves the
ApplianceOS console. An ApplianceOS app is one container, so the database rides
inside it, under the app's `data` volume, until ApplianceOS has companion
containers.

Don't install this and the [wiretap-server app](../wiretap-server/README.md)
on one box: both declare GVRET on tcp 23, and ApplianceOS holds every declared
port. The server app is for capturing on another box and forwarding here.

## Build

```sh
apps/wiretap-gateway/make-image.sh          # arm64 and amd64
apps/wiretap-gateway/make-image.sh arm64    # one of them
```

Each architecture becomes a `docker load` tarball,
`target/apps/wiretap-gateway-<version>-<arch>.tar`, tagged
`ghcr.io/wired-square/wiretap-gateway:<version>`, the gateway's version. The
gateway and the daemon are static musl binaries cross-compiled with
`cargo zigbuild`, as `packaging/make-deb.sh` builds the daemon, and the UI and
its browser half as the [server app](../wiretap-server/README.md) builds them.
So the script needs `apps/wiretap-server/frontend/node_modules`, the two
`*-unknown-linux-musl` Rust targets, `cargo-zigbuild`, `zig`, `docker buildx`
and `jq`, and for arm64 `wslib-appliance-xtask`.

The image's `com.wiredsquare.appliance.manifest` label is `manifest.json` with
the crate's version stamped in, so the label, the tag and
`org.opencontainers.image.version` always agree.

To ship it to a box, pack it into the apps' signed source
([apps/README.md](../README.md)).

## What it keeps

Everything persistent is under `/data`, the image's only volume:

| Path | What |
| --- | --- |
| `/data/pg` | the database cluster |
| `/data/secrets/postgres-password` | generated on first start, mode 0600 |
| `/data/secrets/admin-key` | generated on first start, mode 0600 |
| `/data/secrets/ingest-key` | the daemon's key to the gateway, generated on first start, mode 0600 |
| `/data/ui.toml`, `/data/cache*.db` | the daemon's settings from a UI, and its disk cache |
| `/data/wiretap-server-ui.db` | the web UI's database, on ApplianceOS |

The first start logs where the admin key is, never the key. Make keys for the
desktop and the capture daemons in the admin UI's Keys tab, on 8423 under
`/admin/`, or on ApplianceOS in the console's Gateway tab, under Keys. Read the admin key with `docker exec <container> cat
/data/secrets/admin-key`. Setting `WIRETAP_ADMIN_KEY` on the container uses
that key instead.

The image points the gateway at both files with `WIRETAP_PG_PASSWORD_FILE` and
`WIRETAP_ADMIN_KEY_FILE`, so either can be rotated from the admin UI's Keys tab
(Secrets), or with `docker exec <container> wiretap-backend rotate
db-password` (or `admin-key`), which prints the new value. The gateway saves it
in the file, and nothing needs a restart. An admin key set with
`WIRETAP_ADMIN_KEY` is rotated for good: put the new one there too, so the env
matches.

Opened from the ApplianceOS console, the app is the web UI, which needs no key:
the console says who you are, the UI passes that on to the gateway, and a
console admin is the gateway's admin. Through the console, rotating a secret,
or creating, rotating or revoking a key, needs your password confirmed in the
console in the last five minutes. The UI's gateway screens are still to come;
until then it shows the Capture screen.

## How it runs

It runs as any uid in group 0. `initdb` has to find its own uid in the
password database, so it runs under `nss_wrapper` with a throwaway entry, and
`/etc` is never written.

`entrypoint.sh` starts Postgres, then the gateway, then the daemon once the
gateway's ingest is listening on 9323, then, on ApplianceOS, the UI. If any of
them exits on its own, the others are stopped and the container exits 1, so a
supervisor sees it down.

On `SIGTERM` it stops them in the reverse order, within ApplianceOS's 30 s
stop timeout:

| Step | Signal | Allowed | Then |
| --- | --- | --- | --- |
| the UI | `SIGTERM` | 5 s | `SIGKILL` |
| the daemon | `SIGTERM` | 15 s | `SIGKILL` |
| the gateway | `SIGTERM` | 5 s | `SIGKILL` |
| Postgres | `SIGINT`, a fast shutdown | the rest | the container's own kill |

The daemon goes first so that it flushes what it holds to the gateway, or to
its disk cache, and the gateway is up to take it. Each normally takes well under
a second. Plain Docker's `docker stop` allows 10 s, so give it `-t 30` (or
`stop_grace_period: 30s`) to keep the same budget.

The daemon is the server app's: the image's
`/etc/wiretap-server/wiretap-server.toml` is that app's file with a
`[forward]` to `127.0.0.1:9323`, into the database `capture`, which the
gateway creates on the daemon's first connection. Its key is
`/data/secrets/ingest-key`, given to the gateway as `WIRETAP_INGEST_KEY_FILE`
and to the daemon as `WIRETAP_FORWARD_TOKEN` on every start. The gateway
honours it for any database without listing it in the Keys tab; to change it,
delete the file and restart. With no device configured the daemon captures
`can0`, and waits for it while it is absent. Its control socket is in
`/run/wiretap-server/<uid>`, a 0700 directory. The daemon carries the file
capabilities `cap_net_raw,cap_net_bind_service+ep`, so it binds GVRET's port
23 without root; the container must be granted both, and must not run with
`no-new-privileges`, which strips them.

The manifest asks for host networking, so the daemon sees the host's CAN
interfaces, with `NET_RAW` and `NET_BIND_SERVICE`, which ApplianceOS grants a
signed app only. Under host networking a listener is a declaration: nothing is
published, and every declared port is held for the clash check, on or off.

| Listener | Port | Binds | What |
| --- | --- | --- | --- |
| `gvret` | tcp 23, on | all IPv4 addresses, private, link-local and Tailscale clients only | the daemon's GVRET bridge |
| `gateway` | tcp 8423, on | all IPv4 addresses | the gateway's HTTP API and admin UI, for the desktop |
| `ingest` | tcp 9323, on | all IPv4 addresses | the gateway's ingest, for this daemon and daemons elsewhere |
| `device-push` | tcp 9324, off | nothing until turned on | the daemon's listener for devices that push frames |

Postgres listens only on a Unix socket in `/run/wiretap`, never on TCP. The
manifest declares `sockets.app`, so ApplianceOS mounts the app's socket
directory at `/run/appliance`. When it is there, the UI serves the console on
`app.sock`, and its hooks on `hooks.sock`, under `/apps/wiretap-gateway/`, and
ApplianceOS probes `healthz` there, which answers 200 while the UI's database
does. The console is uid 0 inside the container
(`WIRETAP_SERVER_UI_CONSOLE_UID`). The UI reaches the daemon on its control
socket and the gateway on `gateway.sock`, both in `/run/wiretap-server/<uid>`:
the entrypoint gives the gateway that socket as `WIRETAP_APP_SOCKET`, at the
root (`WIRETAP_APP_BASE_PATH=/`), with the app's own uid as
`WIRETAP_CONSOLE_UID`: the gateway believes `Appliance-Identity` from that uid
alone, and the 0700 directory keeps every other uid off the socket. Without
`/run/appliance` the UI does not run.

The `HEALTHCHECK`, for plain Docker, asks the gateway's `/healthz` on 8423,
under `WIRETAP_BASE_PATH`, which answers 200 only while Postgres does.

Postgres runs with these settings over the defaults:

- `dynamic_shared_memory_type=sysv`. Docker gives `/dev/shm` 64 MB, and
  ApplianceOS cannot raise it, so a parallel hash join failed with "could not
  resize shared memory segment" under `posix`. System V segments are not
  bounded by `/dev/shm`, and parallel query stays on.
- `max_worker_processes=32`, because TimescaleDB's background jobs and parallel
  workers draw on the same slots.
- `timescaledb.telemetry_level=off`.

`timescaledb-tune` does not run, so `shared_buffers` stays at 128 MB.

## Outside ApplianceOS

```sh
docker volume create wiretap-gateway
docker run -d --name wiretap-gateway --init --stop-timeout 30 \
  --network host --cap-add NET_RAW --cap-add NET_BIND_SERVICE \
  -v wiretap-gateway:/data ghcr.io/wired-square/wiretap-gateway:<version>
```

Host networking is what lets the daemon see `can0`. On a bridge network,
publish 8423 and 9323 instead; the daemon then waits for a `can0` it never
sees, and forwards nothing.

For `psql`:

```sh
docker exec -it wiretap-gateway sh -c \
  'PGPASSWORD=$(cat /data/secrets/postgres-password) psql -h /run/wiretap -U postgres wiretap'
```
