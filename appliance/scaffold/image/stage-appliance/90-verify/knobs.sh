# Chassis-owned. What appliance.toml says that wiretap-appliance's image has to be true
# of, as the values verify.sh and tests/run-verify-tests.sh both read —
# rendered once so a knob cannot reach one and not the other.
NAME='wiretap-appliance'
BINARY='wiretap-appliance'
GROUP='wiretap-appliance'
CONFIG_DIR='/etc/wiretap-appliance'
STATE_DIR='/var/lib/wiretap-appliance'
HOSTNAME_PREFIX='wiretap'
JOURNALD_DROPIN='/usr/lib/systemd/journald.conf.d/95-wiretap-appliance-persistent.conf'
HOST_ADMIN=1
WIFI=0
BLUETOOTH=0
LINK_LOCAL=1
JOURNAL_PERSISTENT=1
ROOT_KEY=0
FIRST_USER=''
FIRST_USER_KEY=0
PASSWORDLESS_SUDO=0
TAILSCALE=1
