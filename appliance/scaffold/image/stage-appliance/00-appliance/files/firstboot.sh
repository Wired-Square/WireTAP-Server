#!/bin/sh
# Chassis-owned. Gives this board a name a fleet can tell apart.
#
# **This is the machine the identity is made on.** The image is written to many
# cards; this runs once on each of them, so anything derived here is per-board
# and anything derived in the pi-gen stage is not.
set -eu

PREFIX='wiretap'

# Outside every directory the package owns, so a purge cannot delete it and
# leave this unit failing on every boot afterwards. See the unit beside this.
DONE='/var/lib/misc/wiretap-appliance-firstboot.done'

# The board's serial, which is per-unit and stable across reboots. `machine-id`
# is the fallback rather than the first choice: it is regenerated if the image
# ships an empty one, so two cards can briefly agree.
serial=$(sed -n 's/^Serial[[:space:]]*:[[:space:]]*//p' /proc/cpuinfo | tail -1)
[ -n "$serial" ] || serial=$(cat /etc/machine-id 2>/dev/null || echo)
suffix=$(printf '%s' "$serial" | tr -dc '0-9a-f' | tail -c 6)

# A board that will not say who it is still has to boot. A fleet of these
# collides on one name, which is visible and fixable; refusing to start is not.
if [ -z "$suffix" ]; then
    echo "firstboot: no serial and no machine-id; leaving the name alone" >&2
    exit 0
fi

name="$PREFIX-$suffix"
old=$(cat /etc/hostname 2>/dev/null || echo)

printf '%s\n' "$name" > /etc/hostname
hostname "$name"

# `127.0.1.1` is Debian's convention for the machine's own name, and a sudo or a
# hostname lookup with no entry there waits for a timeout it will never satisfy.
if [ -n "$old" ] && grep -q "[[:space:]]$old\$" /etc/hosts; then
    sed -i "s/\([[:space:]]\)$old\$/\1$name/" /etc/hosts
else
    printf '127.0.1.1\t%s\n' "$name" >> /etc/hosts
fi

# The marker the unit's ConditionPathExists reads, written last: if anything
# above failed, `set -e` leaves it absent and the next boot tries again. It
# holds the name, which is what a factory reset returns the box to.
mkdir -p "$(dirname "$DONE")"
printf '%s\n' "$name" > "$DONE"

echo "firstboot: this board is $name"
