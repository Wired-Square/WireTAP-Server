#!/bin/sh
# Chassis-owned. Joins this board to wiretap-appliance's tailnet with the auth key the
# image carries or an Admin gave, and removes the key once it has.
set -eu
umask 077

KEY='/var/lib/misc/wiretap-appliance-tailscale.key'
# What the last attempt said, kept while it fails: the Network screen reads
# its last line.
SAID='/var/lib/misc/wiretap-appliance-tailscale.failed'

# The name the board gave itself at first boot; without one, tailscale takes
# the box's own.
name=$(cat '/var/lib/misc/wiretap-appliance-firstboot.done' 2>/dev/null || true)
set --
[ -z "$name" ] || set -- --hostname="$name"

# `file:` keeps the key out of argv. The card keeps the LAN's resolver whatever
# DNS the login server pushes.
if tailscale up --login-server='https://controlplane.tailscale.com' --auth-key="file:$KEY" --accept-dns=false "$@" 2>"$SAID"; then
	cat "$SAID" >&2
	rm -f "$KEY" "$SAID"
else
	status=$?
	cat "$SAID" >&2
	exit "$status"
fi

echo "tailscale-enrol: joined https://controlplane.tailscale.com${name:+ as $name}"
