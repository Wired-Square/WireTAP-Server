#!/usr/bin/env bash
#
# Take the built .deb through everything dpkg can do to it, on a host with a
# running systemd, asserting at each step.
#
#   packaging/tests/deb-lifecycle.sh --yes target/deb/wiretap-server_0.1.0_amd64.deb
#
# THIS INSTALLS AND PURGES A SYSTEM PACKAGE, creates and deletes a system user,
# and restarts journald. Run it on a throwaway host - a CI runner, a container
# booting systemd, a Pi you do not mind. `--yes` is the acknowledgement.
#
# Why a script and not a CI step: the interesting half of debian/postinst only
# runs when systemd is, so this needs a booted host either way, and one that
# CI and a laptop can both provide. A container is enough:
#
#   docker run -d --name wt --privileged --cgroupns=host \
#     -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock \
#     -v "$PWD/target/deb:/debs:ro" <debian-bookworm-with-systemd> /sbin/init
#
# What it is guarding, in order of what it would cost to find in the field:
#
#   - The daemon deadlocking on SIGTERM, which makes every `systemctl restart`
#     - the postinst's upgrade restart included - wait out TimeoutStopSec, get
#     SIGKILLed, and lose the shutdown flush. Step 4 times the stop.
#   - A cache the daemon can read and not write, which it must refuse to start
#     on rather than capture into nothing. Step 6.
#   - purge taking the capture, or the account that owns it, with it. Step 8.
#   - The Python's unit shadowing this package's, which would report a healthy
#     daemon that had never run. Not here: it needs a Python deployment to
#     displace, so it stays a manual drill.

set -euo pipefail

say() { printf '\n=== %s\n' "$*"; }
ok() { printf '    ok: %s\n' "$*"; }
die() { printf '    FAILED: %s\n' "$*" >&2; exit 1; }

UNIT=wiretap-server.service
CONFIG=/etc/wiretap-server/wiretap-server.toml
STATE=/var/lib/wiretap-server
CACHE="$STATE/cache.db"
# High, so nothing here needs a privileged port or collides with a real one.
INGEST_PORT=9400

if [ "${1:-}" != "--yes" ]; then
	sed -n '2,28p' "$0" | sed 's/^#//;s/^ //'
	exit 2
fi
DEB="${2:?usage: $0 --yes <path-to-.deb>}"
# Callers pass a glob - CI does. If it matched more than one the shell hands
# them all over and only the first would be tested, which after a version bump
# is the *previous* release: a green run against a package nobody built today.
[ "$#" -eq 2 ] || die "expected one package, got $(($# - 1)):
     $(shift; echo "$*")
     A glob matching several is how a stale build gets tested in place of this one."
[ -f "$DEB" ] || die "no such package: $DEB"
# Made absolute before apt ever sees it. `apt-get install a/b` is apt's
# package/release syntax, so a relative path is read as a request for package
# `target` from release `deb` and the file is never opened - "E: Unable to
# locate package target/deb", which names something the caller never typed.
# Every invocation from the repo root hits this, CI included; it went unnoticed
# because every local run so far passed an absolute path.
DEB="$(CDPATH= cd -- "$(dirname "$DEB")" && pwd)/$(basename "$DEB")"
[ -d /run/systemd/system ] || die "no running systemd here; see the header"
[ "$(id -u)" -eq 0 ] || die "must run as root (it installs a package)"

# The reference client, for pushing frames the cache can then be counted for.
CLIENT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/tools/test_ingest_client.py"
[ -f "$CLIENT" ] || die "missing $CLIENT"
# Named here rather than discovered halfway through a run that has already
# installed a package: sqlite3 counts the cache, python3 pushes the frames.
for t in sqlite3 python3; do
	command -v "$t" >/dev/null || die "$t not found (apt-get install $t)"
done

frames() { sqlite3 "$CACHE" 'select count(*) from frames;' 2>/dev/null || echo 0; }
active() { systemctl is-active --quiet "$UNIT"; }

# Leave the host as it was found, whatever happens after this point.
cleanup() {
	systemctl stop "$UNIT" >/dev/null 2>&1 || true
	systemctl reset-failed "$UNIT" >/dev/null 2>&1 || true
	apt-get purge -y wiretap-server >/dev/null 2>&1 || true
	rm -rf "$STATE"
	deluser --quiet --system wiretap >/dev/null 2>&1 || true
	delgroup --quiet --system wiretap >/dev/null 2>&1 || true
}
trap cleanup EXIT

cleanup  # a previous run, or a real install, would make every assertion a lie

# --- 1. a fresh install ---------------------------------------------------
say "1. install"
DEBIAN_FRONTEND=noninteractive apt-get install -y "$DEB" >/dev/null
getent passwd wiretap >/dev/null || die "no wiretap user"
[ "$(stat -c '%U:%a' "$STATE")" = "wiretap:700" ] \
	|| die "$STATE is $(stat -c '%U:%a' "$STATE"), wanted wiretap:700"
[ -f "$CONFIG" ] || die "no $CONFIG"
[ -f /usr/share/wiretap-server/wiretap-server.toml ] || die "no packaged reference config"
[ "$(systemctl is-enabled "$UNIT")" = enabled ] || die "unit not enabled"
# Enabled and NOT started: it cannot usefully run until it is configured, and
# anything else crash-loops in the journal.
! active || die "the unit was started on a fresh install"
ok "user, state directory, config and unit; enabled, not started"

# --- 2. it runs, under the unit's hardening -------------------------------
# Ingest-only, because a runner has no CAN interface. The forward target is a
# closed port on purpose: that is the outage path, so every frame pushed below
# has to reach the disk cache, which is what makes step 8 countable.
say "2. configure and start"
cat > "$CONFIG" <<EOF
[server]
iface = ""
[forward]
enable = true
host = "127.0.0.1"
port = 19323
[ingest]
enable = true
port = $INGEST_PORT
EOF
systemctl start "$UNIT"
sleep 2
active || { journalctl -u "$UNIT" -n 30 --no-pager >&2; die "did not start"; }
[ "$(systemctl show "$UNIT" -p User --value)" = wiretap ] || die "not running as wiretap"
[ -f "$CACHE" ] || die "no $CACHE under StateDirectory="
ok "active as wiretap, cache in $STATE"

# --- 3. frames reach the cache --------------------------------------------
say "3. push frames through the ingest listener"
python3 "$CLIENT" --host 127.0.0.1 --port "$INGEST_PORT" --count 50 >/dev/null
sleep 2
systemctl stop "$UNIT"
cached="$(frames)"
[ "$cached" -gt 0 ] || die "the gateway is down, so the cache should hold frames; it has $cached"
ok "$cached frames cached through a gateway outage"

# --- 4. the stop is graceful ----------------------------------------------
# The regression guard for the SIGTERM deadlock. Under it this took the unit's
# full TimeoutStopSec=30 and then a SIGKILL, every single time, and the
# shutdown flush - the thing that saves the queue - never ran.
say "4. stop, with a device connected"
systemctl start "$UNIT"; sleep 1
python3 "$CLIENT" --host 127.0.0.1 --port "$INGEST_PORT" --count 100000 >/dev/null 2>&1 &
pusher=$!
sleep 3
start_ns="$(date +%s%N)"
systemctl stop "$UNIT"
elapsed_ms=$(( ($(date +%s%N) - start_ns) / 1000000 ))
kill "$pusher" 2>/dev/null || true; wait "$pusher" 2>/dev/null || true
result="$(systemctl show "$UNIT" -p Result --value)"
[ "$result" = success ] || die "stop finished as '$result' (a deadlock shows up as 'timeout')"
# Generous: the point is to separate "flushed and exited" from "waited out
# TimeoutStopSec=30", not to benchmark. It measured single-digit ms.
[ "$elapsed_ms" -lt 5000 ] || die "stop took ${elapsed_ms}ms; the flush is not completing"
journalctl -u "$UNIT" -n 20 --no-pager | grep -q 'closed:' \
	|| die "no shutdown summary in the journal, so the flush did not run"
ok "stopped in ${elapsed_ms}ms, cleanly, with a client attached"

# --- 5. upgrade over a running daemon -------------------------------------
say "5. reinstall while running"
systemctl start "$UNIT"; sleep 1
before="$(systemctl show "$UNIT" -p MainPID --value)"
DEBIAN_FRONTEND=noninteractive apt-get install -y --reinstall "$DEB" >/dev/null
after="$(systemctl show "$UNIT" -p MainPID --value)"
active || die "the daemon did not come back after an upgrade"
[ "$before" != "$after" ] || die "pid $before unchanged; the new binary is not running"
grep -q "port = $INGEST_PORT" "$CONFIG" || die "the upgrade overwrote the configuration"
systemctl stop "$UNIT"
ok "restarted ($before -> $after), configuration untouched"

# --- 6. a cache it can read and not write ---------------------------------
# What a purge and a reinstall used to leave behind when the two installs were
# allocated different uids. Opening such a cache succeeds - SQLite falls back
# to O_RDONLY - so before the daemon checked, it started, looked healthy, and
# dropped every frame.
say "6. an unwritable cache is refused, and the packaging repairs it"
chown 999:999 "$CACHE"*
systemctl start "$UNIT" 2>/dev/null || true
sleep 2
! active || die "started on a cache it cannot write"
journalctl -u "$UNIT" -n 20 --no-pager | grep -q 'read-only' \
	|| die "refused to start but did not say the cache was read-only"
# Stop before reset-failed: Restart=always means it is sitting in a restart
# timer, and clearing the failed state alone would leave that running into the
# reinstall below.
systemctl stop "$UNIT" >/dev/null 2>&1 || true
systemctl reset-failed "$UNIT" >/dev/null 2>&1 || true
DEBIAN_FRONTEND=noninteractive apt-get install -y --reinstall "$DEB" >/dev/null
[ "$(stat -c '%U' "$CACHE")" = wiretap ] || die "the postinst did not re-own the cache"
systemctl start "$UNIT"; sleep 2
active || die "still will not start after the packaging repaired the ownership"
systemctl stop "$UNIT"
ok "refused it, named it, and the reinstall gave it back"

# --- 7. remove keeps what it should ---------------------------------------
say "7. remove"
kept="$(frames)"
DEBIAN_FRONTEND=noninteractive apt-get remove -y wiretap-server >/dev/null
[ ! -f /usr/bin/wiretap-server ] || die "the binary survived a remove"
[ -f "$CONFIG" ] || die "remove took the configuration; only purge may"
[ -f "$CACHE" ] || die "remove took the capture"
[ ! -f /etc/systemd/journald.conf.d/95-wiretap-persistent.conf ] \
	|| die "the journald drop-in survived a remove"
ok "binary gone; configuration, capture and $kept frames kept"

# --- 8. purge keeps the capture, and the account that owns it -------------
say "8. purge"
DEBIAN_FRONTEND=noninteractive apt-get purge -y wiretap-server >/dev/null
[ ! -d /etc/wiretap-server ] || die "purge left the configuration, which holds the API key"
[ -d "$STATE" ] || die "purge took $STATE, which holds frames that never reached the gateway"
[ "$(frames)" = "$kept" ] || die "purge changed the capture: $kept -> $(frames) frames"
# The account outlives the package because its data does: dpkg reserves no uid,
# so deleting the owner is what makes the kept frames unreadable later.
getent passwd wiretap >/dev/null || die "purge kept the capture and deleted the user that owns it"
ok "$STATE kept, $kept frames intact, wiretap account kept"

# --- 9. and purge with nothing to keep takes the account with it ----------
# The try-it-and-purge host: installed, never started, so postinst created the
# state directory and nothing ever put a cache in it. An empty directory is not
# a capture, so both it and the account should go - which is why the test above
# is `rmdir` and not `[ -d ]`.
say "9. purge after an install that never ran"
rm -rf "$STATE"
DEBIAN_FRONTEND=noninteractive apt-get install -y "$DEB" >/dev/null
[ -d "$STATE" ] || die "postinst did not create $STATE"
[ -z "$(ls -A "$STATE")" ] || die "something wrote to $STATE without the daemon running"
DEBIAN_FRONTEND=noninteractive apt-get purge -y wiretap-server >/dev/null
[ ! -d "$STATE" ] || die "an empty $STATE was kept; only a capture earns that"
! getent passwd wiretap >/dev/null || die "nothing was kept, so the account should have gone"
ok "empty state directory and account both removed"

printf '\n=== all nine steps passed\n'
