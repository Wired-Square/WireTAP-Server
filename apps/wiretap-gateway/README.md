# wiretap-gateway, the ApplianceOS app

One image holding `wiretap-backend` and its own PostgreSQL 16 with TimescaleDB
2.29.2. An ApplianceOS app is one container, so the database rides inside it,
under the app's `data` volume, until ApplianceOS has companion containers.

## Build

```sh
apps/wiretap-gateway/make-image.sh          # arm64 and amd64
apps/wiretap-gateway/make-image.sh arm64    # one of them
```

Each architecture becomes a `docker load` tarball,
`target/apps/wiretap-gateway-<version>-<arch>.tar`, tagged
`ghcr.io/wired-square/wiretap-gateway:<version>`. The gateway is a static musl
binary cross-compiled with `cargo zigbuild`, as `packaging/make-deb.sh` builds
the daemon, so the script needs `cargo-zigbuild`, `zig`, the two
`*-unknown-linux-musl` Rust targets, `docker buildx` and `jq`.

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

The first start logs where the admin key is, never the key. Opened from the
ApplianceOS console, the admin UI needs no key: the console says who you are,
and a console admin is the gateway's admin. Make keys for the desktop and the
capture daemons in its Keys tab. Outside ApplianceOS, read the admin key with
`docker exec <container> cat /data/secrets/admin-key`. Setting
`WIRETAP_ADMIN_KEY` on the container uses that key instead.

The image points the gateway at both files with `WIRETAP_PG_PASSWORD_FILE` and
`WIRETAP_ADMIN_KEY_FILE`, so either can be rotated from the admin UI's Keys tab
(Secrets), or with `docker exec <container> wiretap-backend rotate
db-password` (or `admin-key`), which prints the new value. The gateway saves it
in the file, and nothing needs a restart. An admin key set with
`WIRETAP_ADMIN_KEY` is rotated for good: put the new one there too, so the env
matches.

## How it runs

It runs as any uid in group 0, with no capabilities. `initdb` has to find its
own uid in the password database, so it runs under `nss_wrapper` with a
throwaway entry, and `/etc` is never written.

`entrypoint.sh` starts Postgres, then the gateway. Postgres listens only on a
Unix socket in `/run/wiretap`, never on TCP. On `SIGTERM` the gateway stops
first, then Postgres with a fast shutdown. If either exits on its own, the
other is stopped and the container exits 1, so a supervisor sees it down.

The gateway listens on TCP 8423 (HTTP) and 9323 (ingest) at the root, for the
desktop and the capture daemons, and on `/run/appliance/app.sock` under
`/apps/wiretap-gateway`, for the console's proxy. The manifest declares
`sockets.app`, so ApplianceOS mounts the app's socket directory there and
probes `/apps/wiretap-gateway/healthz` over it. The image sets
`WIRETAP_APP_SOCKET`, `WIRETAP_APP_BASE_PATH` and `WIRETAP_CONSOLE_UID=0`, the
uid the console has inside the container.

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
docker run -d --name wiretap-gateway --init \
  -v wiretap-gateway:/data -p 8423:8423 -p 9323:9323 \
  ghcr.io/wired-square/wiretap-gateway:<version>
```

For `psql`:

```sh
docker exec -it wiretap-gateway sh -c \
  'PGPASSWORD=$(cat /data/secrets/postgres-password) psql -h /run/wiretap -U postgres wiretap'
```
