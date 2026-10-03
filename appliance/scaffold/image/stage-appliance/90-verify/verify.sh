#!/bin/sh
# Chassis-owned. Asserts what wiretap-appliance's image has to be true of, against a root
# filesystem tree.
#
#     verify.sh <rootfs> [checks-dir]
#
# `00-run.sh` beside this runs it against ${ROOTFS_DIR} during the build, where
# a failure aborts the build; `tests/run-verify-tests.sh` runs it against trees
# it makes. Every failure is collected rather than the first stopping the run,
# and each check prints a stable id, so the harness can assert which one fired.
#
# **Host-side file facts only, in POSIX sh.** It runs on a laptop under the
# harness as well as in the build, so no `stat -c`, no `readlink -f`, no `sed
# -i`, and nothing shells into the chroot: what sshd would apply is read by
# `00-run.sh` with `sshd -G`, and here a first-wins directive is resolved
# lexically — which assumes the Include is sshd_config's first directive and
# is asserted — because that is the only form a fixture can test.
#
# The knobs are `knobs.sh`, rendered from appliance.toml, and the checks follow
# them: a product that ships Wi-Fi on is not told its Wi-Fi is on. What the
# toml did not ask for but the build may add — a developer's key from
# config.local — arrives as the names the rendered `config` exports, pi-gen's
# `PUBKEY_SSH_FIRST_USER` and `00-appliance`'s `PUBKEY_SSH_ROOT`, and the fleet
# key as `TAILSCALE_KEYED=1` from `90-verify/00-run.sh`; a key neither source
# asked for fails.
#
# A product's own checks are `*.sh` files in the checks directory — `checks.d/`
# beside this in the build — each defining `check`, run here with `$R`, `fail`
# and `ok` in scope; `fixture`, which the harness runs to put what `check`
# needs into its good tree; and `cases`, which the harness runs to prove
# `check` fires. A file without all three fails by its name.

R="${1:?usage: verify.sh <rootfs> [checks-dir]}"
CHECKS="${2:-}"
[ -d "$R" ] || { echo "verify: no such root: $R" >&2; exit 2; }

. "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/knobs.sh"
ROOT_MAY=$ROOT_KEY; [ -z "${PUBKEY_SSH_ROOT:-}" ] || ROOT_MAY=1
FIRST_USER_MAY=$FIRST_USER_KEY; [ -z "${PUBKEY_SSH_FIRST_USER:-}" ] || FIRST_USER_MAY=1

# So a glob's order is glob(3)'s and `sort` agrees with it.
export LC_ALL=C

# `fail` also tallies on fd 9, read back from each product file's subshell
# below; opened here so a failure anywhere else, or on an fd 9 inherited from
# the harness, counts nowhere.
exec 9>/dev/null
fails=0
fail() { echo "  $1 FAIL: $2"; echo fail >&9; fails=$((fails + 1)); }
ok()   { echo "  $1 ok:   $2"; }

# Whether a symlink points at /dev/null, which is what `systemctl mask` writes.
masked() { [ -L "$1" ] && [ "$(readlink "$1")" = "/dev/null" ]; }

# Whether unit $1 is wanted by multi-user.target.
enabled() { [ -L "$R/etc/systemd/system/multi-user.target.wants/$1" ]; }

# Whether a glob matched: its first word is a file (a dangling symlink too),
# not the pattern.
any() { [ -e "$1" ] || [ -L "$1" ]; }

# Whether account $2 is in group $1.
in_group() { grep -qE "^$1:.*[:,]$2(,|\$)" "$R/etc/group" 2>/dev/null; }

# Whether the sshd_config-shaped file $1 sets directive $2 to a value matching $3.
says() { grep -qiE "^[[:space:]]*$2[[:space:]=]+$3" "$1"; }

# The drop-ins sorting before $1 in its directory that set a directive
# matching $2 to something other than $3, as sshd would read them: its Include
# globs `*.conf` and keeps the first value it sees, so a lower-sorting file
# wins outright — an earlier file that agrees is not a pre-emption.
earlier_setting() {
	found=""
	for f in "${1%/*}"/*.conf; do
		[ -f "$f" ] || continue
		[ "$f" = "$1" ] && break
		grep -iE "^[[:space:]]*$2[[:space:]=]+" "$f" | grep -qviE "^[[:space:]]*$2[[:space:]=]+$3[[:space:]]*\$" &&
			found="$found ${f##*/}"
	done
	printf '%s' "${found# }"
}

# Whether account $1 has a NOPASSWD rule in sudoers.d, for itself or via
# `%sudo`. Files sudo ignores — a name with a `.` or ending in `~` — are
# skipped as sudo skips them. This is the shape recognised; a `Defaults
# !authenticate` or an edit to /etc/sudoers itself is not.
nopasswd_sudo() {
	for f in "$R"/etc/sudoers.d/*; do
		[ -f "$f" ] || continue
		case "${f##*/}" in *.* | *~) continue ;; esac
		if grep -qE "^[[:space:]]*$1[[:space:]]+.*NOPASSWD" "$f"; then
			printf '%s' "${f##*/}"; return 0
		fi
		if grep -qE "^[[:space:]]*%sudo[[:space:]]+.*NOPASSWD" "$f" && in_group sudo "$1"; then
			printf '%s (via %%sudo)' "${f##*/}"; return 0
		fi
	done
	return 1
}

SSHD_D="$R/etc/ssh/sshd_config.d"
SSHD_CONF="$R/etc/ssh/sshd_config"

echo "verify: $R"

# --- VERIFY-01: sshd reads its drop-ins first ----------------------------------
# The Include has to be the first directive, or a line above it in sshd_config
# wins over every drop-in and nothing below can see it.
first=$(grep -vE '^[[:space:]]*(#|$)' "$SSHD_CONF" 2>/dev/null | head -n 1)
if [ ! -d "$SSHD_D" ]; then
	fail VERIFY-01 "/etc/ssh/sshd_config.d does not exist"
elif ! printf '%s\n' "$first" | grep -qiE '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/'; then
	fail VERIFY-01 "sshd_config's first directive is not the Include of /etc/ssh/sshd_config.d/ (it is '${first:-nothing}')"
else
	ok VERIFY-01 "sshd_config.d exists and is included first"
fi

# --- VERIFY-02: no card takes a password over ssh -----------------------------
# The chassis's drop-in holds the policy for every card, and sshd keeps the
# first value it reads — so the file has to be there and nothing may sort
# before it and say otherwise. A Match block overrides the global value for
# the connections it matches and `sshd -G` evaluates none, so none is allowed.
POLICY="$SSHD_D/00-$NAME.conf"
hit=$(earlier_setting "$POLICY" '(PasswordAuthentication|KbdInteractiveAuthentication)' no)
matches=$(grep -liE '^[[:space:]]*Match[[:space:]]' "$SSHD_CONF" "$SSHD_D"/*.conf 2>/dev/null | sed "s|^$R||" | tr '\n' ' ')
if [ ! -f "$POLICY" ]; then
	fail VERIFY-02 "00-$NAME.conf is missing from sshd_config.d - a card would take a password"
elif ! says "$POLICY" PasswordAuthentication no || ! says "$POLICY" KbdInteractiveAuthentication no; then
	fail VERIFY-02 "00-$NAME.conf does not set both PasswordAuthentication no and KbdInteractiveAuthentication no"
elif [ -n "$hit" ]; then
	fail VERIFY-02 "a drop-in sorting before 00-$NAME.conf pre-empts the password policy: $hit"
elif [ -n "$matches" ]; then
	fail VERIFY-02 "a Match block, which can re-enable what the policy turned off for the connections it matches: ${matches% }"
else
	ok VERIFY-02 "00-$NAME.conf holds the password policy and nothing pre-empts it"
fi

# Three checks only where the browser manages the keys and the radios, which
# is a host-admin daemon.
if [ "$HOST_ADMIN" = 1 ]; then
	# --- VERIFY-03: sshd reads the keys the daemon manages ------------------
	# The package's postinst wrote this drop-in and could only warn about it:
	# `sshd -t` wants host keys the chroot does not have, and a pre-empting
	# drop-in is its owner's.
	KEYS="$SSHD_D/50-$NAME.conf"
	hit=$(earlier_setting "$KEYS" AuthorizedKeysFile ".*$CONFIG_DIR/ssh/%u")
	if [ ! -f "$KEYS" ]; then
		fail VERIFY-03 "50-$NAME.conf is missing - the postinst did not write it, or something removed it"
	elif ! says "$KEYS" AuthorizedKeysFile ".*$CONFIG_DIR/ssh/%u"; then
		fail VERIFY-03 "50-$NAME.conf does not point AuthorizedKeysFile at $CONFIG_DIR/ssh/%u"
	elif [ -n "$hit" ]; then
		fail VERIFY-03 "a drop-in sorting before 50-$NAME.conf sets AuthorizedKeysFile elsewhere, so the keys the daemon writes are never read: $hit"
	else
		ok VERIFY-03 "50-$NAME.conf names $CONFIG_DIR/ssh/%u and nothing pre-empts it"
	fi

	# --- VERIFY-04: no radio is switched off where the browser cannot undo it
	# /boot/firmware/config.txt, not /boot/config.txt: pi-gen writes a "do not
	# edit" decoy at the latter, which a grep would pass vacuously.
	CFG="$R/boot/firmware/config.txt"
	if [ ! -f "$CFG" ]; then
		fail VERIFY-04 "$CFG does not exist (has the boot path moved?)"
	elif grep -qE '^[[:space:]]*dtoverlay=(disable-wifi|disable-bt)' "$CFG"; then
		fail VERIFY-04 "config.txt disables a radio in the device tree - the Network screen cannot undo that"
	else
		ok VERIFY-04 "no disable-wifi/disable-bt overlay"
	fi

	# --- VERIFY-05: Wi-Fi can be turned back on ---------------------------
	# NetworkManager drives Wi-Fi through wpa_supplicant; masked, the switch
	# in the browser does nothing.
	if masked "$R/etc/systemd/system/wpa_supplicant.service"; then
		fail VERIFY-05 "wpa_supplicant.service is masked - Wi-Fi could never be turned back on"
	else
		ok VERIFY-05 "wpa_supplicant.service is not masked"
	fi
fi

# --- VERIFY-06: Wi-Fi as appliance.toml says -----------------------------------
# NetworkManager's own state file, which is what the Network screen's switch
# rewrites. pi-gen writes WirelessEnabled=false only when WPA_COUNTRY is unset.
STATE="$R/var/lib/NetworkManager/NetworkManager.state"
if [ "$WIFI" = 0 ]; then
	if [ ! -f "$STATE" ]; then
		fail VERIFY-06 "$STATE does not exist - Wi-Fi is on by default"
	elif ! grep -qE '^WirelessEnabled=false[[:space:]]*$' "$STATE"; then
		fail VERIFY-06 "WirelessEnabled is not false (did a substage after 00-appliance rewrite the state file?)"
	else
		ok VERIFY-06 "WirelessEnabled=false"
	fi
elif grep -qsE '^WirelessEnabled=false[[:space:]]*$' "$STATE"; then
	fail VERIFY-06 "WirelessEnabled=false, but appliance.toml says wifi = true"
else
	ok VERIFY-06 "Wi-Fi is left on, as appliance.toml says"
fi

# --- VERIFY-07: Bluetooth as appliance.toml says -------------------------------
# Off is masked, not disabled: bluetooth.service is bus-activated and a
# merely disabled unit is started by the first client. The real unit has to
# exist for the mask to mean anything, and `systemctl mask` does not check.
BT="$R/etc/systemd/system/bluetooth.service"
if [ ! -f "$R/usr/lib/systemd/system/bluetooth.service" ]; then
	ok VERIFY-07 "bluez is not installed, so there is no bluetooth.service to mask"
elif [ "$BLUETOOTH" = 0 ]; then
	if masked "$BT"; then
		ok VERIFY-07 "bluetooth.service is masked"
	else
		fail VERIFY-07 "bluetooth.service is not masked (disabled is not enough - it is bus-activated)"
	fi
elif masked "$BT"; then
	fail VERIFY-07 "bluetooth.service is masked, but appliance.toml says bluetooth = true"
else
	ok VERIFY-07 "Bluetooth is left on, as appliance.toml says"
fi

# --- VERIFY-08: no host key baked in -------------------------------------------
# stage2 removed the ones the ssh package made; a substage that made a set
# again - for a postinst's `sshd -t`, say - would ship one identity on every
# card.
keys=""
for k in "$R"/etc/ssh/ssh_host_*; do
	[ -e "$k" ] && keys="$keys ${k##*/}"
done
if [ -n "$keys" ]; then
	fail VERIFY-08 "SSH host keys in the image:${keys}"
else
	ok VERIFY-08 "no host keys"
fi

# --- VERIFY-09: something makes the host keys on first boot --------------------
# raspberrypi-sys-mods' regenerate_ssh_host_keys.service, enabled by its own
# postinst; which target wants it moved between bookworm and trixie, so any
# `*.target.wants` counts. Without it a key-only card has no sshd and no way in.
RK=regenerate_ssh_host_keys.service
wants=$(ls -d "$R"/etc/systemd/system/*.target.wants/"$RK" "$R"/usr/lib/systemd/system/*.target.wants/"$RK" 2>/dev/null | head -n 1)
wants=${wants%/*}; wants=${wants##*/}
if [ ! -f "$R/usr/lib/systemd/system/$RK" ]; then
	fail VERIFY-09 "$RK is not installed - nothing will make host keys, and sshd will not start"
elif [ -z "$wants" ]; then
	fail VERIFY-09 "$RK is installed but no target wants it - the keys would never be made"
else
	ok VERIFY-09 "$RK is installed and enabled ($wants)"
fi

# --- VERIFY-10: no password, on any account ------------------------------------
# A password in the image is the same credential on every card. The field has
# to be one of the four spellings of "none": `*` and `!` are what base-passwd
# and adduser write, `!*` what systemd-sysusers writes and what either lock -
# `usermod -L`, `passwd -l` - makes of `*`, and `!!` a password never set on
# other distributions. Anything else is a hash - either lock over a hash leaves
# `!<hash>`, one `passwd -u` from working - or empty, which Debian's `pam_unix
# nullok` accepts at the console this image keeps enabled.
if [ ! -f "$R/etc/shadow" ]; then
	fail VERIFY-10 "/etc/shadow does not exist"
else
	passworded=$(awk -F: '$2 != "*" && $2 != "!" && $2 != "!!" && $2 != "!*" { printf " %s", $1 }' "$R/etc/shadow")
	if [ -n "$passworded" ]; then
		fail VERIFY-10 "account(s) with a password, or an empty one:$passworded - the same credential on every card"
	else
		ok VERIFY-10 "no account has a password"
	fi
fi

# --- VERIFY-11: the way in, if there is one, was asked for and reaches the socket
# What appliance.toml asked for has to be there; what config.local added is
# allowed by the names the rendered config exports; a key on any other account
# was asked for by nobody. A root key needs sshd enabled and nothing else. A
# first user with a key needs the login shell pi-gen withholds from a
# passwordless account, and a way to the socket - passwordless sudo, or
# membership of the appliance's group - unless root has a key or the toml
# chose otherwise.
IFS=: read -r U _ _ _ _ uhome ushell <<EOF
$(awk -F: '$3 == 1000' "$R/etc/passwd" 2>/dev/null)
EOF
ukeys=${U:+"$R${uhome:-/home/$U}/.ssh/authorized_keys"}
rkeys="$R/root/.ssh/authorized_keys"
sudo_grant=$(nopasswd_sudo "$U" || true)
unasked=""
while IFS=: read -r name _ _ _ _ home _; do
	for f in "$R$home/.ssh/authorized_keys" "$R$home/.ssh/authorized_keys2"; do
		[ -s "$f" ] || continue
		case "$name" in
		root) [ "$ROOT_MAY" = 1 ] && [ "$f" = "$rkeys" ] && continue ;;
		"$U") [ "$FIRST_USER_MAY" = 1 ] && [ "$f" = "$ukeys" ] && continue ;;
		esac
		unasked="$unasked ${f#"$R"}"
	done
done < "$R/etc/passwd"
for f in "$R"/etc/skel/.ssh/*; do
	[ -e "$f" ] && unasked="$unasked ${f#"$R"}"
done
no_login() { case "$1" in */nologin | */false | "") ;; *) return 1 ;; esac; }
if [ -n "$unasked" ]; then
	fail VERIFY-11 "a key nobody asked for:${unasked} - appliance.toml and config.local name the only keys a card may carry (a key removed from config.local stays in a reused rootfs; clean.sh first)"
elif [ "$ROOT_KEY" = 1 ] && [ ! -s "$rkeys" ]; then
	fail VERIFY-11 "appliance.toml sets root_ssh_key, but /root/.ssh/authorized_keys is missing or empty"
elif [ "$FIRST_USER_KEY" = 1 ] && [ "$U" != "$FIRST_USER" ]; then
	fail VERIFY-11 "appliance.toml names first_user = $FIRST_USER, but uid 1000 is '${U:-nobody}'"
elif [ "$FIRST_USER_KEY" = 1 ] && [ ! -s "$ukeys" ]; then
	fail VERIFY-11 "appliance.toml sets first_user_ssh_key, but $FIRST_USER has no authorized_keys"
elif [ "$PASSWORDLESS_SUDO" = 1 ] && [ -z "$sudo_grant" ]; then
	fail VERIFY-11 "appliance.toml sets passwordless_sudo, but nothing in /etc/sudoers.d grants $U NOPASSWD"
elif { [ -s "$rkeys" ] || [ -s "$ukeys" ]; } && ! enabled ssh.service; then
	fail VERIFY-11 "a key is installed but ssh.service is not enabled - no way in (config.local wants ENABLE_SSH=1)"
elif [ ! -s "$ukeys" ] && [ -s "$rkeys" ]; then
	ok VERIFY-11 "root signs in by key ($(grep -c . "$rkeys") key(s))"
elif [ ! -s "$ukeys" ]; then
	ok VERIFY-11 "no SSH way in: a shipped card (a developer's wants PUBKEY_SSH_ROOT in config.local)"
elif no_login "$ushell"; then
	fail VERIFY-11 "$U has a key but its shell is '${ushell:-unset}' - sshd would accept the key and refuse the session"
elif [ -n "$sudo_grant" ]; then
	ok VERIFY-11 "$U signs in by key with passwordless sudo ($sudo_grant)"
elif in_group "$GROUP" "$U"; then
	ok VERIFY-11 "$U signs in by key and is in the $GROUP group, which opens the socket"
elif [ -s "$rkeys" ]; then
	ok VERIFY-11 "$U signs in by key with no sudo; root signs in by key"
elif [ "$FIRST_USER_KEY" = 1 ]; then
	ok VERIFY-11 "$U signs in by key and, as appliance.toml says, has no sudo - the socket is out of its reach"
else
	fail VERIFY-11 "$U has a key and no passwordless sudo - it can sign in and do nothing (config.local wants PASSWORDLESS_SUDO=1)"
fi

if [ "$JOURNAL_PERSISTENT" = 1 ]; then
	# --- VERIFY-12: the journal survives the reboot it records ------------
	# The *merged* Storage=, resolved as systemd resolves it: every drop-in
	# under /usr/lib, /usr/local/lib, /run and /etc, one file per name with
	# the highest directory winning, sorted by file name, last one wins.
	# Raspberry Pi OS ships 40-rpi-volatile-storage.conf, which beats a 10-
	# name; the package's is 95-.
	journald() {
		printf '%s\n' "$JOURNALD_MERGED" | sed -nE "s/^[[:space:]]*$1=([A-Za-z0-9]+).*/\\1/p" | tail -n 1
	}
	JOURNALD_MERGED=$(
		for d in "$R/etc/systemd/journald.conf.d" "$R/run/systemd/journald.conf.d" \
			"$R/usr/local/lib/systemd/journald.conf.d" "$R/usr/lib/systemd/journald.conf.d"; do
			[ -d "$d" ] || continue
			for f in "$d"/*.conf; do
				# `-L` as well: a /dev/null symlink masks a lower file and says nothing.
				{ [ -f "$f" ] || [ -L "$f" ]; } && printf '%s\t%s\n' "${f##*/}" "$f"
			done
		done | awk -F'\t' '!seen[$1]++' | sort -k1,1 | cut -f2 | xargs cat 2>/dev/null
	)
	storage=$(journald Storage)
	if [ ! -f "$R$JOURNALD_DROPIN" ]; then
		fail VERIFY-12 "the package's ${JOURNALD_DROPIN##*/} is not under /usr/lib/systemd/journald.conf.d"
	elif [ "$storage" != persistent ]; then
		fail VERIFY-12 "the winning journald Storage= is '${storage:-unset}', not persistent - a drop-in sorting after ${JOURNALD_DROPIN##*/}, or one of its name under /etc, wins"
	elif [ -z "$(journald SystemMaxUse)" ]; then
		fail VERIFY-12 "no SystemMaxUse - the package's cap did not survive the merge"
	else
		ok VERIFY-12 "journald resolves to persistent and capped"
	fi
fi

# --- VERIFY-13: the link-local address as appliance.toml says ------------------
LL="$R/etc/NetworkManager/conf.d/10-$NAME-link-local.conf"
if [ "$LINK_LOCAL" = 1 ]; then
	if [ ! -f "$LL" ]; then
		fail VERIFY-13 "10-$NAME-link-local.conf is missing - a box on a bare cable would have no address"
	elif ! grep -qE '^ipv4\.link-local=3[[:space:]]*$' "$LL"; then
		fail VERIFY-13 "10-$NAME-link-local.conf does not set ipv4.link-local=3"
	else
		ok VERIFY-13 "every connection takes a 169.254 address beside its lease"
	fi
elif [ -e "$LL" ]; then
	fail VERIFY-13 "10-$NAME-link-local.conf is present, but appliance.toml says link_local = false"
else
	ok VERIFY-13 "no link-local default, as appliance.toml says"
fi

# --- VERIFY-14: the package is in, and starts ----------------------------------
# A whole unit earlier on systemd's path than /usr/lib replaces ours outright
# and a /dev/null link masks it; only a drop-in merges.
replaced=""
for dir in etc/systemd/system.control etc/systemd/system etc/systemd/system.attached \
	usr/local/lib/systemd/system; do
	for unit in "$NAME.service" "$NAME-firstboot.service"; do
		! any "$R/$dir/$unit" || replaced="$replaced /$dir/$unit"
	done
done
if [ ! -x "$R/usr/bin/$BINARY" ]; then
	fail VERIFY-14 "/usr/bin/$BINARY is missing or not executable - the package did not install"
elif [ ! -f "$R/usr/lib/systemd/system/$NAME.service" ]; then
	fail VERIFY-14 "$NAME.service is not under /usr/lib/systemd/system"
elif ! enabled "$NAME.service"; then
	fail VERIFY-14 "$NAME.service is not enabled"
elif [ ! -f "$R/usr/lib/systemd/system/$NAME-firstboot.service" ] || [ ! -x "$R/usr/libexec/$NAME-firstboot" ]; then
	fail VERIFY-14 "the first-boot unit or its script is missing"
elif ! enabled "$NAME-firstboot.service"; then
	fail VERIFY-14 "$NAME-firstboot.service is not enabled - every card would be called $HOSTNAME_PREFIX"
elif [ -n "$replaced" ]; then
	fail VERIFY-14 "a unit replaces or masks the package's:$replaced - a change to a unit goes in a drop-in"
else
	ok VERIFY-14 "$BINARY installed; $NAME and its first-boot unit enabled"
fi

# --- VERIFY-15: nothing per-board was made on the build host -------------------
# The stage runs once and its output is written to every card; the first-boot
# unit and the daemon's first start are where a name, a pair and a database
# come from. The state directory is systemd's to make at first start, and the
# keys directory is the daemon's, so anything in either was made here.
read -r named < "$R/etc/hostname" 2>/dev/null || named=""
state=$(ls -A "$R$STATE_DIR" 2>/dev/null | tr '\n' ' ')
made=""
[ ! -e "$R/var/lib/misc/$NAME-firstboot.done" ] || made="$made the first-boot marker;"
[ "$named" = "$HOSTNAME_PREFIX" ] || made="$made /etc/hostname is '$named', not $HOSTNAME_PREFIX;"
[ -z "$state" ] || made="$made $STATE_DIR is not empty: $state;"
[ -z "$(ls -A "$R$CONFIG_DIR/ssh" 2>/dev/null)" ] || made="$made keys under $CONFIG_DIR/ssh;"
[ -z "$(ls -A "$R/var/lib/tailscale" 2>/dev/null)" ] || made="$made a Tailscale node's state in /var/lib/tailscale;"
if [ -n "$made" ]; then
	fail VERIFY-15 "made on the build host, so shared by every card:${made%;}"
else
	ok VERIFY-15 "no marker, no name, nothing in the state directory, no keys, no Tailscale node"
fi

# --- VERIFY-16: .local names resolve where the module is -----------------------
if ! any "$R"/usr/lib/*/libnss_mdns4_minimal.so.2; then
	ok VERIFY-16 "libnss-mdns is not installed, so nsswitch.conf is left alone"
elif grep -qE '^hosts:.*mdns' "$R/etc/nsswitch.conf" 2>/dev/null; then
	ok VERIFY-16 "nsswitch.conf resolves .local through libnss-mdns"
else
	fail VERIFY-16 "libnss-mdns is installed but nsswitch.conf's hosts line does not name it"
fi

# --- VERIFY-17: no credential at the console -----------------------------------
# build-image.sh skips the account wizard and cloud-init with pi-gen's SKIP
# markers and refuses to start if their directories have moved; what reaches
# here is a product substage that enabled `userconfig.service`, installed
# cloud-init, or ran `raspi-config do_boot_behaviour`, which writes an
# autologin drop-in. Each is a root-capable console with nothing to type, on
# an image whose getty stays enabled on purpose.
console=""
for f in "$R"/etc/systemd/system/getty@tty1.service.d/*.conf; do
	[ -f "$f" ] && grep -q -- '--autologin' "$f" && console="$console autologin in ${f##*/};"
done
! any "$R"/etc/systemd/system/*.wants/userconfig.service || console="$console userconfig.service (the account wizard) enabled;"
[ ! -x "$R/usr/bin/cloud-init" ] || console="$console cloud-init installed, which reads user-data off the boot partition;"
if [ -n "$console" ]; then
	fail VERIFY-17 "a way in with nothing to type:${console%;}"
else
	ok VERIFY-17 "no autologin, no account wizard, no cloud-init"
fi

# --- VERIFY-18: the daemon, and the Tailscale enrolment, after time-sync.target
# The rendered unit's own `[Unit]`, not a drop-in: systemd cannot empty a
# dependency list, so no drop-in can take this away, and a product may rely on it.
timed_units="$NAME.service"
[ "$TAILSCALE" = 0 ] || timed_units="$timed_units $NAME-tailscale-enrol.service"
for timed in $timed_units; do
	UNIT="$R/usr/lib/systemd/system/$timed"
	if [ ! -f "$UNIT" ]; then
		fail VERIFY-18 "$timed is not under /usr/lib/systemd/system"
	elif ! awk '/^[[:space:]]*\[/ { unit = /^[[:space:]]*\[Unit\]/ }
		unit && /^[[:space:]]*After[[:space:]]*=(.*[[:space:]])?time-sync\.target([[:space:]]|$)/ { found = 1 }
		END { exit !found }' "$UNIT"; then
		fail VERIFY-18 "$timed's [Unit] has no After=time-sync.target - it was not rendered by the chassis, or a substage rewrote it"
	else
		ok VERIFY-18 "$timed is ordered after time-sync.target"
	fi
done

# --- VERIFY-19: Tailscale as appliance.toml says --------------------------------
# With no log upload to Tailscale: no file but ours may name the variable —
# tailscaled's own drop-ins, the drop-ins for every service, a whole unit in
# /etc, or /etc/default/tailscaled. A file an EnvironmentFile= names is not
# followed.
TAILSCALED="$R/usr/lib/systemd/system/tailscaled.service"
TS_DROPIN="$R/etc/systemd/system/tailscaled.service.d/$NAME.conf"
ts_others=""
for f in "$R"/etc/systemd/system/tailscaled.service.d/*.conf "$R"/usr/lib/systemd/system/tailscaled.service.d/*.conf \
	"$R"/etc/systemd/system/service.d/*.conf "$R"/usr/lib/systemd/system/service.d/*.conf \
	"$R/etc/systemd/system/tailscaled.service" "$R/etc/default/tailscaled"; do
	[ "$f" != "$TS_DROPIN" ] && grep -qs TS_NO_LOGS_NO_SUPPORT "$f" && ts_others="$ts_others ${f#"$R"}"
done
if [ "$TAILSCALE" = 0 ]; then
	if [ -e "$R/usr/sbin/tailscaled" ] || [ -e "$R/usr/bin/tailscale" ] || [ -e "$TAILSCALED" ]; then
		fail VERIFY-19 "Tailscale is installed, but appliance.toml has no [image.tailscale] (a reused rootfs? clean.sh first)"
	else
		ok VERIFY-19 "no Tailscale, as appliance.toml says"
	fi
elif [ ! -x "$R/usr/sbin/tailscaled" ] || [ ! -x "$R/usr/bin/tailscale" ] || [ ! -f "$TAILSCALED" ]; then
	fail VERIFY-19 "appliance.toml asks for Tailscale, but it is not installed"
elif ! enabled tailscaled.service; then
	fail VERIFY-19 "tailscaled.service is not enabled - no card would reach its tailnet"
elif ! grep -qxE 'Environment=TS_NO_LOGS_NO_SUPPORT=true[[:space:]]*' "$TS_DROPIN" 2>/dev/null; then
	fail VERIFY-19 "tailscaled.service.d/$NAME.conf is missing or does not set TS_NO_LOGS_NO_SUPPORT=true - tailscaled would upload its logs to Tailscale"
elif [ -n "$ts_others" ]; then
	fail VERIFY-19 "another file sets TS_NO_LOGS_NO_SUPPORT for tailscaled and can undo $NAME.conf:$ts_others"
else
	ok VERIFY-19 "Tailscale installed, tailscaled enabled, and no log upload"
fi

# --- VERIFY-20: the fleet key, only where config.local set one, and root's alone
# Owned as the rootfs's own / is, which in the build is root:root and in the
# harness's tree is whoever made it.
# The enrolment is enabled with Tailscale, key or none, so a key an Admin gives
# later is retried at every boot.
TKEY="$R/var/lib/misc/$NAME-tailscale.key"
root_owner=$(ls -lnd "$R" | awk '{ print $3, $4 }')
enrols() {
	[ -f "$R/usr/lib/systemd/system/$NAME-tailscale-enrol.service" ] && [ -x "$R/usr/libexec/$NAME-tailscale-enrol" ] &&
		enabled "$NAME-tailscale-enrol.service"
}
if [ "${TAILSCALE_KEYED:-0}" = 0 ]; then
	if [ -e "$TKEY" ] || [ -L "$TKEY" ]; then
		fail VERIFY-20 "a fleet key nobody asked for, /var/lib/misc/$NAME-tailscale.key (a reused rootfs? clean.sh first)"
	elif [ "$TAILSCALE" = 1 ] && ! enrols; then
		fail VERIFY-20 "$NAME-tailscale-enrol.service is not installed and enabled - an auth key put on the card later would never be tried"
	else
		ok VERIFY-20 "no fleet key: no card enrols by itself"
	fi
elif [ ! -f "$TKEY" ]; then
	fail VERIFY-20 "config.local sets TAILSCALE_AUTH_KEY, but /var/lib/misc/$NAME-tailscale.key is missing"
elif [ "$(ls -lnd "$TKEY" | awk '{ print substr($1, 1, 10), $3, $4 }')" != "-rw------- $root_owner" ]; then
	fail VERIFY-20 "/var/lib/misc/$NAME-tailscale.key is not 0600 root:root"
elif ! enrols; then
	fail VERIFY-20 "the fleet key is in, but $NAME-tailscale-enrol.service is not installed and enabled to use it"
else
	ok VERIFY-20 "the fleet key is root's alone and $NAME-tailscale-enrol.service will use it"
fi

# --- VERIFY-21: a host-admin daemon may write what Tailscale's controls write
SANDBOX="$R/etc/systemd/system/$NAME.service.d/tailscale.conf"
if [ "$TAILSCALE" = 0 ] || [ "$HOST_ADMIN" = 0 ]; then
	ok VERIFY-21 "no Tailscale controls to open the sandbox for"
elif ! grep -qxE 'ReadWritePaths=/var/lib/misc -/var/lib/tailscale[[:space:]]*' "$SANDBOX" 2>/dev/null; then
	fail VERIFY-21 "$NAME.service.d/tailscale.conf is missing or does not open /var/lib/misc and /var/lib/tailscale - the Network screen's Tailscale controls would fail"
elif [ ! -d "$R/var/lib/tailscale" ]; then
	fail VERIFY-21 "/var/lib/tailscale is missing - it would be read-only to the daemon"
else
	ok VERIFY-21 "the daemon may write the auth key's directory and tailscaled's state"
fi

# --- A product's own checks ------------------------------------------------------
# Each file in a subshell, its failures counted from its fd 9 tally, so nothing
# it assigns — `fails` included — reaches this shell. The tally is a file, not
# a pipe, so a process a check leaves running cannot hold the build open.
if [ -d "$CHECKS" ]; then
	tally=$(mktemp)
	trap 'rm -f "$tally"' EXIT
	for file in "$CHECKS"/*.sh; do
		[ -f "$file" ] || continue
		echo "verify: ${file##*/}"
		(
			unset -f check fixture cases
			. "$file"
			for fn in check fixture cases; do
				command -v "$fn" >/dev/null 2>&1 || fail "${file##*/}" "defines no $fn function - a checks.d file defines check, fixture and cases"
			done
			! command -v check >/dev/null 2>&1 || check
			echo finished >&9
		) 9>"$tally"
		fails=$((fails + $(awk '$0 == "fail" { n++ } END { print n + 0 }' "$tally")))
		[ "$(tail -n 1 "$tally")" = finished ] || fail "${file##*/}" "stopped before its check finished"
	done
fi

# ---------------------------------------------------------------------------------
if [ "$fails" -ne 0 ]; then
	echo "verify: $fails check(s) failed"
	exit 1
fi
echo "verify: all checks passed"
