#!/usr/bin/env bash
#
# Take wiretap-server-ui's .deb through install, upgrade, remove and purge
# beside the daemon's, on a host with a running systemd, asserting at each step.
#
#   apps/wiretap-server/tests/deb-lifecycle.sh --yes \
#     target/deb/wiretap-server_<v>_arm64.deb \
#     apps/wiretap-server/target/debian/deb/wiretap-server-ui_<v>_arm64.deb
#
# THIS INSTALLS AND PURGES TWO SYSTEM PACKAGES and creates and deletes system
# users. Run it on a throwaway host, such as the container
# packaging/tests/deb-lifecycle.sh describes, with curl and sqlite3 in
# it. `--yes` is the acknowledgement.

set -euo pipefail

say() { printf '\n=== %s\n' "$*"; }
ok() { printf '    ok: %s\n' "$*"; }
die() { printf '    FAILED: %s\n' "$*" >&2; exit 1; }

DAEMON=wiretap-server.service
UI=wiretap-server-ui.service
CONFIG=/etc/wiretap-server/wiretap-server.toml
DAEMON_SOCKET=/run/wiretap-server/control.sock
UI_SOCKET=/run/wiretap-server-ui/wiretap-server-ui.sock
UI_STATE=/var/lib/wiretap-server-ui
UI_DB="$UI_STATE/wiretap-server-ui.db"

if [ "${1:-}" != "--yes" ]; then
	sed -n '2,13p' "$0" | sed 's/^#//;s/^ //'
	exit 2
fi
[ "$#" -eq 3 ] || die "expected the daemon's package and the UI's, got $(($# - 1))"
absolute() { echo "$(CDPATH= cd -- "$(dirname "$1")" && pwd)/$(basename "$1")"; }
DAEMON_DEB="$(absolute "$2")"
UI_DEB="$(absolute "$3")"
for deb in "$DAEMON_DEB" "$UI_DEB"; do [ -f "$deb" ] || die "no such package: $deb"; done
[ -d /run/systemd/system ] || die "no running systemd here"
[ "$(id -u)" -eq 0 ] || die "must run as root (it installs packages)"
[ ! -e /usr/sbin/policy-rc.d ] || die "/usr/sbin/policy-rc.d is here, and stops a package starting its unit; remove it"
for t in curl sqlite3; do
	command -v "$t" >/dev/null || die "$t not found (apt-get install $t)"
done

apt_get() { DEBIAN_FRONTEND=noninteractive apt-get -y "$@" >/dev/null; }
active() { systemctl is-active --quiet "$1"; }
main_pid() { systemctl show "$1" -p MainPID --value; }
# Over the UI's control socket, which is Admin to root.
ui_api() { curl -sS --max-time 40 --unix-socket "$UI_SOCKET" "$@"; }
mode_and_group() { stat -c '%a %G' "$1"; }
gid() { getent group "$1" | cut -d: -f3; }
sorted() { xargs -n1 | sort -n | xargs; }
https_up() {
	for _ in $(seq 1 40); do
		[ "$(curl -sk --max-time 2 -o /dev/null -w '%{http_code}' https://127.0.0.1:8443/healthz)" = 200 ] && return 0
		sleep 0.5
	done
	return 1
}
capture_ready() { ui_api http://localhost/api/capture/status | grep -q '"state":"ready"'; }

cleanup() {
	systemctl stop "$UI" "$DAEMON" >/dev/null 2>&1 || true
	apt-get purge -y wiretap-server-ui wiretap-server >/dev/null 2>&1 || true
	rm -rf /var/lib/wiretap-server "$UI_STATE"
	for account in wiretap-server-ui wiretap; do
		deluser --quiet --system "$account" >/dev/null 2>&1 || true
		delgroup --quiet --system "$account" >/dev/null 2>&1 || true
	done
}
trap cleanup EXIT
cleanup

# --- 1. both installed ------------------------------------------------------
say "1. install the daemon, then the UI"
apt_get install "$DAEMON_DEB"
apt_get install "$UI_DEB"
[ "$(id -Gn wiretap-server-ui)" = wiretap-server-ui ] \
	|| die "wiretap-server-ui is in $(id -Gn wiretap-server-ui); only its own group was wanted"
[ -z "$(getent group wiretap-server-ui | cut -d: -f4)" ] \
	|| die "the UI's group has members: $(getent group wiretap-server-ui)"
[ "$(systemctl is-enabled "$UI")" = enabled ] || die "the UI's unit is not enabled"
active "$UI" || { journalctl -u "$UI" -n 30 --no-pager >&2; die "the UI did not start"; }
https_up || die "nothing answered https://127.0.0.1:8443/healthz"
ok "installed, enabled, started, HTTPS answering"

# --- 2. its groups and its socket -----------------------------------------
say "2. the UI reaches the daemon's group and nothing wider reaches the UI"
want="$(echo "$(gid wiretap-server-ui) $(gid wiretap)" | sorted)"
got="$(awk '/^Groups:/ { $1 = ""; print }' "/proc/$(main_pid "$UI")/status" | sorted)"
[ "$got" = "$want" ] || die "the UI runs with groups '$got', wanted '$want'"
[ "$(mode_and_group "$UI_SOCKET")" = "660 wiretap-server-ui" ] \
	|| die "$UI_SOCKET is $(mode_and_group "$UI_SOCKET")"
[ "$(mode_and_group "$(dirname "$UI_SOCKET")")" = "750 wiretap-server-ui" ] \
	|| die "$(dirname "$UI_SOCKET") is $(mode_and_group "$(dirname "$UI_SOCKET")")"
[ "$(stat -c '%U %a' "$UI_STATE")" = "wiretap-server-ui 700" ] \
	|| die "$UI_STATE is $(stat -c '%U %a' "$UI_STATE")"
[ -f "$UI_DB" ] || die "no $UI_DB"
! runuser -u wiretap -- curl -s --max-time 5 --unix-socket "$UI_SOCKET" http://localhost/healthz >/dev/null 2>&1 \
	|| die "the daemon's own user reached the UI's control socket"
ok "groups {wiretap-server-ui, wiretap}; socket 0660 in a 0750 directory; state 0700"

# --- 3. a change through the UI re-executes the daemon ----------------------
say "3. the UI shows the daemon and changes a setting"
cat > "$CONFIG" <<EOF
[server]
iface = ""
[forward]
enable = true
host = "127.0.0.1"
port = 19323
[ingest]
enable = true
port = 9400
EOF
systemctl start "$DAEMON"
for _ in $(seq 1 20); do [ -S "$DAEMON_SOCKET" ] && break; sleep 0.5; done
capture_ready || die "the UI does not see the daemon: $(ui_api http://localhost/api/capture/status)"
ui_api -o /dev/null -w '%{http_code}' http://localhost/api/users -X POST \
	-H 'content-type: application/json' \
	-d '{"username":"admin","password":"lifecycle test password","role":"admin"}' | grep -qE '^20[01]$' \
	|| die "could not make the first account over the UI's control socket"
daemon_pid="$(main_pid "$DAEMON")"
code="$(ui_api -o /dev/null -w '%{http_code}' http://localhost/api/capture/settings -X PUT \
	-H 'content-type: application/json' -d '{"logging.level":"debug"}')"
[ "$code" = 200 ] || die "the change answered $code"
grep -q 'level = "debug"' /var/lib/wiretap-server/ui.toml || die "ui.toml does not hold the change"
[ "$(main_pid "$DAEMON")" = "$daemon_pid" ] || die "the daemon's PID moved; it was restarted, not re-executed"
[ "$(systemctl show "$DAEMON" -p NRestarts --value)" = 0 ] || die "systemd restarted the daemon"
ok "status ready, first account made, a change applied on the same PID"

# --- 4. upgrades ------------------------------------------------------------
say "4. reinstall the UI, then the daemon, while both run"
pid="$(main_pid "$UI")"
cert="$(sha256sum "$UI_STATE/cert.pem")"
apt_get install --reinstall "$UI_DEB"
active "$UI" || die "the UI did not come back after its upgrade"
[ "$(main_pid "$UI")" != "$pid" ] || die "the UI's PID $pid is unchanged; the new binary is not running"
[ "$(sha256sum "$UI_STATE/cert.pem")" = "$cert" ] || die "the upgrade replaced the UI's certificate"
[ "$(sqlite3 "$UI_DB" 'select count(*) from users;')" = 1 ] || die "the upgrade lost the account"
https_up || die "HTTPS did not come back after the upgrade"
apt_get install --reinstall "$DAEMON_DEB"
for _ in $(seq 1 20); do capture_ready && break; sleep 0.5; done
capture_ready || die "the UI lost the daemon across the daemon's upgrade"
ok "each restarted on its upgrade; the account, the certificate and the link kept"

# --- 5. remove and purge the UI, leaving the daemon -------------------------
say "5. remove the UI"
apt_get remove wiretap-server-ui
[ ! -e /usr/bin/wiretap-server-ui ] || die "the binary survived a remove"
! active "$UI" || die "the UI is still running"
[ -f "$UI_DB" ] || die "remove took the UI's accounts; only purge may"
active "$DAEMON" || die "removing the UI stopped the daemon"
ok "binary gone and stopped; accounts kept; the daemon still running"

say "6. purge the UI"
apt_get purge wiretap-server-ui
[ ! -d "$UI_STATE" ] || die "purge left $UI_STATE"
[ ! -d /etc/wiretap-server-ui ] || die "purge left /etc/wiretap-server-ui"
! getent passwd wiretap-server-ui >/dev/null || die "purge left the wiretap-server-ui user"
! getent group wiretap-server-ui >/dev/null || die "purge left the wiretap-server-ui group"
getent group wiretap >/dev/null || die "purging the UI took the daemon's group"
active "$DAEMON" || die "purging the UI stopped the daemon"
ok "state, configuration and account gone; the daemon and its group untouched"

# --- 7. the daemon cannot go without the UI ---------------------------------
say "7. remove the daemon under an installed UI"
apt_get install "$UI_DEB"
apt_get remove wiretap-server
dpkg-query -W -f='${db:Status-Status}' wiretap-server-ui | grep -qx config-files \
	|| die "the UI stayed installed without the daemon it depends on"
apt_get purge wiretap-server-ui wiretap-server
! getent passwd wiretap-server-ui >/dev/null || die "the final purge left the UI's user"
ok "apt took the UI with the daemon, and purged both"

printf '\n=== all seven steps passed\n'
