# Chassis-owned. What appliance.toml says that wiretap-appliance's image has to be true
# of, as the values verify.sh and tests/run-verify-tests.sh both read —
# rendered once so a knob cannot reach one and not the other.
NAME='wiretap-appliance'
PACKAGE='wiretap-appliance'
BINARY='wiretap-appliance'
ENV_PREFIX='WIRETAP_APPLIANCE'
GROUP='wiretap-appliance'
CONFIG_DIR='/etc/wiretap-appliance'
STATE_DIR='/var/lib/wiretap-appliance'
HOSTNAME_PREFIX='wiretap'
LOCAL_ALIAS=1
CONSOLE_ART=0
RELEASE='trixie'
JOURNALD_DROPIN='/usr/lib/systemd/journald.conf.d/95-wiretap-appliance-persistent.conf'
HOST_ADMIN=1
HATS=0
BOOT_FIRMWARE_DROPIN='/usr/lib/systemd/system/wiretap-appliance.service.d/50-boot-firmware.conf'
WIFI=0
BLUETOOTH=0
LINK_LOCAL=1
JOURNAL_PERSISTENT=1
ROOT_KEY=0
FIRST_USER=''
FIRST_USER_KEY=0
PASSWORDLESS_SUDO=0
TAILSCALE=1
