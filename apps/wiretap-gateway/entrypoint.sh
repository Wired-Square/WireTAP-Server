#!/bin/sh
# Postgres, then the gateway on its Unix socket; stopped in the reverse order.
set -eu

PGDATA=/data/pg
SECRETS=/data/secrets
SOCKETS=/run/wiretap

log() { echo "wiretap-gateway: $*" >&2; }

stop() {
	kill -TERM "${gw:-}" 2>/dev/null && wait "$gw" || :
	kill -INT "${pg:-}" 2>/dev/null && wait "$pg" || :
}
trap 'stop; exit 0' TERM INT

random_hex() { od -An -tx1 -N32 /dev/urandom | tr -d ' \n'; }
keep() { printf '%s' "$2" >"$1.part" && mv "$1.part" "$1"; }

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
[ -s "$SECRETS/postgres-password" ] || keep "$SECRETS/postgres-password" "$(random_hex)"
if [ ! -s "$SECRETS/admin-key" ]; then
	keep "$SECRETS/admin-key" "wt_$(random_hex)"
	log "generated the admin key, in $SECRETS/admin-key"
fi

if [ ! -d "$PGDATA" ]; then
	log "initialising the database in $PGDATA"
	rm -rf "$PGDATA.new"
	with_passwd_entry initdb --pgdata="$PGDATA.new" --username=postgres \
		--pwfile="$SECRETS/postgres-password" --auth=scram-sha-256 --encoding=UTF8 >/dev/null
	mv "$PGDATA.new" "$PGDATA"
fi

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

POSTGRES_PASSWORD=$(cat "$SECRETS/postgres-password") \
	WIRETAP_ADMIN_KEY=${WIRETAP_ADMIN_KEY:-$(cat "$SECRETS/admin-key")} \
	wiretap-backend &
gw=$!

# Polled: busybox's `wait -n` misses a child that dies while it waits.
while kill -0 "$pg" 2>/dev/null && kill -0 "$gw" 2>/dev/null; do
	sleep 1 &
	wait $! || :
done
log "a process exited on its own; stopping the rest"
stop
exit 1
