# wiretap-server-ui

The capture daemon's web UI: the chassis's shell and a Capture screen, which
shows what `wiretap-server` is doing and changes the settings a UI may change.
It reaches the daemon only through the daemon's control socket (the root
README's § Settings from a UI), so the daemon stays headless and installs
without it.

This is a cargo workspace of its own on the Wired Square appliance chassis
([`wslib-appliance-rs`](https://github.com/Wired-Square/wslib-appliance-rs),
pinned at `v0.8.0`), which is a private repository, as `appliance/` is. The
root workspace and its release build without it.

| Path | What |
| --- | --- |
| `crates/wiretap-server-ui` | the binary |
| `frontend` | its browser half: the chassis's shell and the Capture screen |

## Two ways to serve

One binary, chosen at start the chassis's way:

- **Standalone** (`appliance-https`): its own HTTPS, accounts and sign-in, with
  a certificate it signs for itself on first start. The redirect is off.
  `WIRETAP_SERVER_UI_ONBOARDING=browser` lets the first person to reach it make
  the first account; without it, the UI's own control socket makes it.
- **Behind an ApplianceOS console** (`appliance-server`): set
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

**Behind a console**, with `appliance-xtask dev-console` standing in for one:

```sh
WIRETAP_SERVER_UI_APP_SOCKET=/tmp/wt/app.sock WIRETAP_SERVER_UI_CONSOLE_UID=$(id -u) \
  WIRETAP_SERVER_UI_BASE_PATH=/apps/wiretap-server/ \
  cargo run -- --db /tmp/wt/ui.db --ui frontend/dist --daemon-socket /tmp/wt/control.sock
appliance-xtask dev-console --app-socket /tmp/wt/app.sock --listen 127.0.0.1:18080
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

## The API

Under `/api`, beside the chassis's:

| Route | Who | What |
| --- | --- | --- |
| `GET /api/capture/status` | any role | the daemon's `GET /v1/status` |
| `GET /api/capture/settings` | any role | the daemon's `GET /v1/settings` |
| `PUT /api/capture/settings` | admin | the daemon's `PUT /v1/settings`, answered with the settings after the restart |

A read answers 200 with the daemon's body and `"state": "ready"`, or
`{"state": "unsupported", "because": "…"}` when the daemon is not answering.
A change the daemon could not be asked is a 503; one it took but has not come
back from within 30 s is a 504.

## Gates

From this directory, with `CARGO_NET_GIT_FETCH_WITH_CLI=true` for the private
chassis:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
(cd frontend && npm ci && npm run ci)    # typecheck, unit tests, build
```

The static `aarch64-unknown-linux-musl` build needs the cross environment in
[docs/workspaces.md](../../docs/workspaces.md#cross-building-the-appliance-on-a-mac):

```sh
cargo build --release --locked --target aarch64-unknown-linux-musl
```
