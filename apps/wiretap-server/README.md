# wiretap-server-ui

The capture daemon's web UI: the chassis's shell and a Capture screen, which
shows what `wiretap-server` is doing and changes the settings a UI may change.
It reaches the daemon only through the daemon's control socket (the root
README's § Settings from a UI), so the daemon stays headless and installs
without it.

This is a cargo workspace of its own on the Wired Square appliance chassis,
the `wslib-appliance-*` crates from the private `wired-square` registry, and
in the browser `@wired-square/appliance-ui` from its npm mirror. The root
workspace and its release build without it.

| Path | What |
| --- | --- |
| `crates/wiretap-server-ui` | the binary |
| `frontend` | its browser half: the chassis's shell and the Capture screen |

## Two ways to serve

One binary, chosen at start the chassis's way:

- **Standalone** (`wslib-appliance-https`): its own HTTPS, accounts and
  sign-in, with a certificate it signs for itself on first start. The redirect is off.
  `WIRETAP_SERVER_UI_ONBOARDING=browser` lets the first person to reach it make
  the first account; without it, the UI's own control socket makes it. Each
  account may add TOTP, with recovery codes, and passkeys on the Users screen;
  with TOTP on, a password sign-in asks for a code next. A passkey needs the UI
  opened by a name, not an IP address, with a certificate the browser trusts.
  An administrator's change to an account asks for their own password again,
  or a code or a passkey while their TOTP is on. A password reset, by an
  administrator or over the control socket, clears the account's second
  factors.
- **Behind an ApplianceOS console** (`wslib-appliance-server`): set
  `WIRETAP_SERVER_UI_APP_SOCKET` and `WIRETAP_SERVER_UI_CONSOLE_UID`, and it
  serves on that Unix socket under `WIRETAP_SERVER_UI_BASE_PATH` (for an app,
  `/apps/<name>/`) to whoever the console's `Appliance-Identity` header names.

Either way the daemon is `--daemon-socket` or `WIRETAP_SERVER_UI_DAEMON_SOCKET`,
by default `/run/wiretap-server/control.sock`, and this process must be in the
daemon's group to open it. Every other setting is the chassis's, under the
`WIRETAP_SERVER_UI_` prefix; `--help` lists the flags.

Anyone signed in reads the status and the settings. Only an admin changes them
(a console's `admin` or `hostadmin`). A change is passed to the daemon as it
is, and the daemon's refusal comes back as it was: a pinned setting is a 409
naming where it is fixed, an unknown key or a transmitting device a 400, and a
change that would stop the daemon starting a 422. An accepted change is
answered once the daemon has restarted on it, with the settings it now runs
on. A daemon that is not running is shown as such, not as an error.

## The package

`wiretap-server-ui`, a `.deb` that depends on `wiretap-server` and leaves that
package as it is. `wslib-appliance-xtask` renders it from
[appliance.toml](appliance.toml) into `scaffold/` (`packaging render`;
`packaging diff` says whether anything there was edited). Two files there are
this repository's own: `debian/copyright`, and the unit's drop-in
`systemd/wiretap-server-ui.service.d/10-daemon-socket.conf`.

```sh
(cd frontend && npm ci && npm run build)
CARGO_BUILD_TARGET=aarch64-unknown-linux-musl scaffold/packaging/make-deb.sh --target deb
```

That needs the cross environment below, and writes
`target/debian/deb/wiretap-server-ui_<version>_<arch>.deb`.

It runs as its own user, `wiretap-server-ui`, in its own group. The drop-in
adds the daemon's group, `wiretap`, to the unit and to nothing else, which is
what opens `/run/wiretap-server/control.sock` to it. Its own control socket,
`/run/wiretap-server-ui/wiretap-server-ui.sock`, can change the daemon's
settings, and is 0660 in that group of one, so only root reaches it, never more
than the daemon's group. It serves HTTPS on 8443 with a certificate it makes on
first start, and keeps its accounts in `/var/lib/wiretap-server-ui`, which only
a purge removes. Make the first account as root:

```sh
sudo curl -s --unix-socket /run/wiretap-server-ui/wiretap-server-ui.sock \
  http://localhost/api/users -X POST -H 'content-type: application/json' \
  -d '{"username":"…","password":"…","role":"admin"}'
```

then sign in at `https://<host>:8443/`.

[tests/deb-lifecycle.sh](tests/deb-lifecycle.sh) installs both packages,
changes a setting through the UI, upgrades each, then removes and purges them,
asserting at each step. Run it somewhere disposable, such as the container
[packaging/tests/deb-lifecycle.sh](../../packaging/tests/deb-lifecycle.sh)
describes, with `/usr/sbin/policy-rc.d` removed.

## Running it on a development machine

Build the browser half once, in `frontend/`:

```sh
npm ci && npm run build
```

Start a daemon with a control socket. Off Linux, the configuration the daemon's
own integration test uses starts anywhere: no CAN, device push only, a gateway
on a dead port, and no config file. From the repository root:

```sh
cargo build -p wiretap-server
mkdir -p /tmp/wt
STATE_DIRECTORY=/tmp/wt target/debug/wiretap-server -C /tmp/wt/absent.toml \
  -i "" --ingest-enable --ingest-host 127.0.0.1 --ingest-port 0 \
  --forward-enable --forward-port 9 --control-socket /tmp/wt/control.sock
```

Keep socket paths short: a Unix socket's path must fit in about 100 bytes.

**Standalone**, from this directory, which then holds its database and
certificate:

```sh
WIRETAP_SERVER_UI_ONBOARDING=browser cargo run -- \
  --bind-address 127.0.0.1 --https-port 8443 --socket /tmp/wt/ui.sock \
  --ui frontend/dist --daemon-socket /tmp/wt/control.sock
```

Open `https://127.0.0.1:8443` and make the first account.

**Behind a console**, with `wslib-appliance-xtask dev-console` standing in for one:

```sh
WIRETAP_SERVER_UI_APP_SOCKET=/tmp/wt/app.sock WIRETAP_SERVER_UI_CONSOLE_UID=$(id -u) \
  WIRETAP_SERVER_UI_BASE_PATH=/apps/wiretap-server/ \
  cargo run -- --db /tmp/wt/ui.db --ui frontend/dist --daemon-socket /tmp/wt/control.sock
wslib-appliance-xtask dev-console --app-socket /tmp/wt/app.sock --listen 127.0.0.1:18080
```

Open `http://127.0.0.1:18080/apps/wiretap-server/`, signed in as the console's
admin. Or ask the socket directly, as the console would:

```sh
curl --unix-socket /tmp/wt/app.sock http://app/apps/wiretap-server/api/capture/status \
  -H 'appliance-identity: v1; id=1; user=admin; role=admin'
```

`npm run dev` in `frontend/` serves the browser half with hot reload, proxied
to the standalone UI on `https://localhost:8443`, or to
`WIRETAP_SERVER_UI_API`, such as the dev console's `http://127.0.0.1:18080`.
It asks for `/api` at the root, so a UI behind the dev console must then be
started without `WIRETAP_SERVER_UI_BASE_PATH`. A passkey runs only on the port
the UI itself serves, so try one there rather than through the dev server.

## The API

Under `/api`, beside the chassis's:

| Route | Who | What |
| --- | --- | --- |
| `GET /api/capture/status` | any role | the daemon's `GET /v1/status` |
| `GET /api/capture/settings` | any role | the daemon's `GET /v1/settings` |
| `PUT /api/capture/settings` | admin | the daemon's `PUT /v1/settings`, answered with the settings after the restart |

A read answers 200 with the daemon's body and `"state": "ready"`, or
`{"state": "unsupported", "because": "…"}` when the daemon is not answering.
A change the daemon could not be asked, or did not answer within 10 s, is a
503; one it took but has not come back from within 30 s is a 504.

## The image

One image holds the daemon and this UI, for plain Docker and for ApplianceOS:

```sh
apps/wiretap-server/make-image.sh          # arm64 and amd64, from the repository root
apps/wiretap-server/make-image.sh arm64    # one of them
```

Each architecture becomes a `docker load` tarball,
`target/apps/wiretap-server-<version>-<arch>.tar`, tagged
`ghcr.io/wired-square/wiretap-server:<version>`, the daemon's version. The
daemon is built with `cargo zigbuild`, as its package is, and the UI through
the rendered `scaffold/packaging/build-binary.sh`, linked with `rust-lld`. The
script needs `frontend/node_modules`, the `*-unknown-linux-musl` targets,
`cargo-zigbuild`, `zig`, `docker buildx` and `jq`, and for arm64
`wslib-appliance-xtask`, which checks the Cortex-A53 erratum. Its label is
`manifest.json` with the version stamped in.

To ship it to an ApplianceOS box, pack it into the apps' signed source
([apps/README.md](../README.md)).

`entrypoint.sh` starts the daemon, then the UI. On `SIGTERM` the UI stops
first. If either exits on its own, the other is stopped and the container
exits 1.

It runs as any uid in group 0. The daemon carries the file capabilities
`cap_net_raw,cap_net_bind_service+ep`, so it binds GVRET's port 23 without
root; the container must be granted both, and must not run with
`no-new-privileges`, which strips them. Both control sockets go in
`/run/wiretap-server/<uid>`, a 0700 directory: under `<uid>:0` the sockets'
group is every process's, so the directory is the boundary. Everything
persistent is under `/data`, the only volume: the daemon's `ui.toml` and disk
cache, the UI's database, and in standalone mode its certificate.
`/etc/wiretap-server/wiretap-server.toml` in the image sets the device-push
listener's port to 9324, so a UI shows it as fixed.

**On ApplianceOS** the manifest asks for host networking, so the daemon sees
the host's CAN interfaces, with `NET_RAW` and `NET_BIND_SERVICE`. When the
console mounts its socket directory at `/run/appliance`, the UI serves the
console on `app.sock` under `/apps/wiretap-server/`, which is also where
ApplianceOS probes `healthz`. Under host networking a listener is a
declaration only: ApplianceOS publishes nothing, and holds every declared
port for clash checks whether it is on or off. So GVRET is declared on tcp 23,
on, and the device-push listener on tcp 9324, off, clear of the gateway app's
9323. The listener stays off until the UI or a config file turns it on.

Without `/run/appliance` the UI serves its own HTTPS on 8443, and the image's
`HEALTHCHECK` asks it for `/healthz`.

## Gates

From this directory, with `wslib-appliance-xtask` at the version CI pins:

```sh
wslib-appliance-xtask gates
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --all-targets --locked --target aarch64-unknown-linux-musl -- -D warnings
cargo test --locked
(cd frontend && npm ci && npm run ci)    # typecheck, unit tests, build
```

The static `aarch64-unknown-linux-musl` build needs the cross environment in
[docs/workspaces.md](../../docs/workspaces.md#cross-building-the-uis-package-on-a-mac):

```sh
cargo build --release --locked --target aarch64-unknown-linux-musl
```

## On Docker

[docker-compose.yml](docker-compose.yml) runs the image with host networking,
`NET_RAW` and `NET_BIND_SERVICE` over `cap_drop: [ALL]`, a named volume on
`/data`, and the standalone UI:

```sh
docker load -i target/apps/wiretap-server-<version>-<arch>.tar
export WIRETAP_SERVER_VERSION=<version>
docker compose -f apps/wiretap-server/docker-compose.yml up -d
```

It sets `WIRETAP_SERVER_UI_ONBOARDING=browser`, so the first person to reach
`https://<host>:8443/` makes the admin account; remove it once that is you.
The daemon's settings come from the UI, beneath the image's
`/etc/wiretap-server/wiretap-server.toml`; mount a file of your own there to
pin more, or to move the device-push listener back to 9323. The UI's own
`WIRETAP_SERVER_UI_*` variables, such as `WIRETAP_SERVER_UI_HTTPS_PORT`, go in
`environment:`. Only the daemon holds a capability, so the UI's port must be
1024 or above.
