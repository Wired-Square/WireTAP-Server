#!/bin/sh
# Postgres, then the gateway, then the capture daemon once the gateway's ingest
# listens, then, behind an ApplianceOS console, the web UI; stopped in the
# reverse order.
set -eu

PGDATA=/data/pg
SECRETS=/data/secrets
SOCKETS=/run/wiretap
INGEST_PORT=9323

log() { echo "wiretap-gateway: $*" >&2; }

# Polled: busybox's `wait -n` misses a child that dies while it waits.
tick() {
	sleep "$1" &
	wait $! || :
}

alive() {
	for pid; do kill -0 "$pid" 2>/dev/null || return 1; done
}

# busybox's kill takes an empty pid for 0, the whole process group.
halt() {
	[ -n "$2" ] || return 0
	kill -TERM "$2" 2>/dev/null || return 0
	n=$(($3 * 10))
	while alive "$2" && [ "$n" -gt 0 ]; do
		tick 0.1
		n=$((n - 1))
	done
	if alive "$2"; then
		log "$1 did not stop within $3 s; killing it"
		kill -KILL "$2"
	fi
	wait "$2" || :
}

stop() {
	halt ui "${ui:-}" 5
	halt daemon "${daemon:-}" 15
	halt gateway "${gw:-}" 5
	[ -z "${pg:-}" ] || { kill -INT "$pg" 2>/dev/null && wait "$pg"; } || :
}
trap 'stop; exit 0' TERM INT

fail() {
	log "a process exited on its own; stopping the rest"
	stop
	exit 1
}

random_hex() { od -An -tx1 -N32 /dev/urandom | tr -d ' \n'; }
keep() { printf '%s' "$2" >"$1.part" && mv "$1.part" "$1"; }

ingest_listening() {
	awk -v port=":$(printf '%04X' "$INGEST_PORT")" \
		'$4 == "0A" && substr($2, length($2) - 4) == port { found = 1 } END { exit !found }' \
		/proc/net/tcp
}

# initdb looks up its own uid, which the console picks and /etc does not know.
with_passwd_entry() {
	if getent passwd "$(id -u)" >/dev/null; then
		"$@"
		return
	fi
	nss=$(mktemp -d)
	echo "postgres:x:$(id -u):$(id -g)::$PGDATA:/sbin/nologin" >"$nss/passwd"
	echo "root:x:0:" >"$nss/group"
	LD_PRELOAD=/usr/lib/libnss_wrapper.so NSS_WRAPPER_PASSWD="$nss/passwd" \
		NSS_WRAPPER_GROUP="$nss/group" "$@"
	rm -rf "$nss"
}

umask 077
mkdir -p "$SECRETS"
[ -s "$WIRETAP_PG_PASSWORD_FILE" ] || keep "$WIRETAP_PG_PASSWORD_FILE" "$(random_hex)"
if [ ! -s "$WIRETAP_ADMIN_KEY_FILE" ]; then
	keep "$WIRETAP_ADMIN_KEY_FILE" "wt_$(random_hex)"
	log "generated the admin key, in $WIRETAP_ADMIN_KEY_FILE"
fi
[ -s "$WIRETAP_INGEST_KEY_FILE" ] || keep "$WIRETAP_INGEST_KEY_FILE" "wt_$(random_hex)"

if [ ! -d "$PGDATA" ]; then
	log "initialising the database in $PGDATA"
	rm -rf "$PGDATA.new"
	with_passwd_entry initdb --pgdata="$PGDATA.new" --username=postgres \
		--pwfile="$WIRETAP_PG_PASSWORD_FILE" --auth=scram-sha-256 --encoding=UTF8 >/dev/null
	mv "$PGDATA.new" "$PGDATA"
fi

# Under <uid>:0 every process here shares group 0, so the sockets' 0660 is no
# boundary: only this uid may enter their directory.
RUNTIME_DIRECTORY="/run/wiretap-server/$(id -u)"
mkdir -p "$RUNTIME_DIRECTORY"
chmod 0700 "$RUNTIME_DIRECTORY"
GATEWAY_SOCKET="$RUNTIME_DIRECTORY/gateway.sock"

# sysv, not posix: Docker's /dev/shm is 64 MB, and a parallel query outgrows it.
# TimescaleDB's jobs share max_worker_processes with parallel query.
postgres -D "$PGDATA" \
	-c listen_addresses= \
	-c unix_socket_directories="$SOCKETS" \
	-c shared_preload_libraries=timescaledb \
	-c timescaledb.telemetry_level=off \
	-c max_worker_processes=32 \
	-c dynamic_shared_memory_type=sysv &
pg=$!

# The cluster's password is the file's, which the gateway rotates in place.
unset POSTGRES_PASSWORD
# The UI is the gateway's console: the gateway believes its Appliance-Identity.
WIRETAP_APP_SOCKET="$GATEWAY_SOCKET" WIRETAP_APP_BASE_PATH=/ WIRETAP_CONSOLE_UID="$(id -u)" \
	wiretap-backend &
gw=$!

until ingest_listening; do
	alive "$pg" "$gw" || fail
	tick 1
done
# SQLite's temporary files go in the data volume.
STATE_DIRECTORY=/data TMPDIR=/data RUNTIME_DIRECTORY="$RUNTIME_DIRECTORY" \
	WIRETAP_FORWARD_TOKEN="$(cat "$WIRETAP_INGEST_KEY_FILE")" \
	wiretap-server -C /etc/wiretap-server/wiretap-server.toml &
daemon=$!

# ApplianceOS mounts the app's socket directory here; plain Docker does not, and
# the gateway's own admin UI on 8423 serves instead.
if awk '$5 == "/run/appliance" { found = 1 } END { exit !found }' /proc/self/mountinfo; then
	STATE_DIRECTORY=/data TMPDIR=/data CONFIGURATION_DIRECTORY=/etc/wiretap-server-ui \
		WIRETAP_SERVER_UI_APP_SOCKET=/run/appliance/app.sock \
		WIRETAP_SERVER_UI_HOOKS_SOCKET=/run/appliance/hooks.sock \
		WIRETAP_SERVER_UI_CONSOLE_UID="${WIRETAP_SERVER_UI_CONSOLE_UID:-0}" \
		WIRETAP_SERVER_UI_BASE_PATH=/apps/wiretap-gateway/ \
		WIRETAP_SERVER_UI_DAEMON_SOCKET="$RUNTIME_DIRECTORY/control.sock" \
		WIRETAP_SERVER_UI_GATEWAY_SOCKET="$GATEWAY_SOCKET" \
		wiretap-server-ui &
	ui=$!
fi

while alive "$pg" "$gw" "$daemon" ${ui:+"$ui"}; do
	tick 1
done
fail
