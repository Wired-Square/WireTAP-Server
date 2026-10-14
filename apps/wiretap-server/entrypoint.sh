#!/bin/sh
# The capture daemon, then its web UI; stopped in the reverse order.
set -eu

log() { echo "wiretap-server: $*" >&2; }

stop() {
	kill -TERM "${ui:-}" 2>/dev/null && wait "$ui" || :
	kill -TERM "${daemon:-}" 2>/dev/null && wait "$daemon" || :
}
trap 'stop; exit 0' TERM INT

umask 077

# Under <uid>:0 every process here shares group 0, so the control sockets'
# 0660 is no boundary: only this uid may enter their directory.
RUNTIME_DIRECTORY="/run/wiretap-server/$(id -u)"
export RUNTIME_DIRECTORY
mkdir -p "$RUNTIME_DIRECTORY"
chmod 0700 "$RUNTIME_DIRECTORY"
export WIRETAP_SERVER_UI_DAEMON_SOCKET="$RUNTIME_DIRECTORY/control.sock"

# ApplianceOS mounts the app's socket directory here; plain Docker does not, and
# the UI then serves its own HTTPS.
if awk '$5 == "/run/appliance" { found = 1 } END { exit !found }' /proc/self/mountinfo; then
	export WIRETAP_SERVER_UI_APP_SOCKET=/run/appliance/app.sock \
		WIRETAP_SERVER_UI_HOOKS_SOCKET=/run/appliance/hooks.sock \
		WIRETAP_SERVER_UI_CONSOLE_UID="${WIRETAP_SERVER_UI_CONSOLE_UID:-0}" \
		WIRETAP_SERVER_UI_BASE_PATH=/apps/wiretap-server/
fi

wiretap-server -C /etc/wiretap-server/wiretap-server.toml &
daemon=$!
wiretap-server-ui &
ui=$!

# Polled: busybox's `wait -n` misses a child that dies while it waits.
while kill -0 "$daemon" 2>/dev/null && kill -0 "$ui" 2>/dev/null; do
	sleep 1 &
	wait $! || :
done
log "a process exited on its own; stopping the rest"
stop
exit 1
