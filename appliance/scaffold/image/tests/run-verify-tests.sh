#!/bin/sh
# Chassis-owned. Proves wiretap-appliance's verify.sh without building an image.
#
#     scaffold/image/tests/run-verify-tests.sh
#
# Runs on macOS and on Debian, needs no root and no board, and takes a few
# seconds. Run it before spending half an hour on a build.
#
# **One good tree, made here, and one mutation per assertion.** A stored broken
# tree proves the checks fire together; it cannot prove they fire independently
# — an inverted check, or a typo in one grep, still comes out red. So the tree
# every check passes on is written fresh for each case, exactly one thing is
# broken, and the run has to fail with exactly that id and no other: that is
# what shows no check masks another. The tree is made rather than kept as
# fixture files because it follows the same knobs the checks do; the files the
# stage and the package install verbatim — the sshd drop-in, the link-local
# default, the journald drop-in, the unit — are copied from the rendered files,
# and the rest are stand-ins in the shape the checks read.
#
# A product's own checks in `stage-appliance/90-verify/checks.d/` run here
# too: each file's `fixture` is run with `$T` the tree's root and `$SCAFFOLD`
# the rendered scaffold, before its `check`; and its `cases` proves the
# check's own failure arms with `case_run` and `case_pass`.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SCAFFOLD=$(dirname -- "$(dirname -- "$HERE")")
STAGE="$SCAFFOLD/image/stage-appliance"
VERIFY="$STAGE/90-verify/verify.sh"
CHECKS="$STAGE/90-verify/checks.d"
[ -f "$VERIFY" ] || { echo "run-verify-tests: no verify.sh at $VERIFY" >&2; exit 2; }
. "$STAGE/90-verify/knobs.sh"

TMP=$(mktemp -d)
readonly TMP
trap 'rm -rf "$TMP"' EXIT
# A key in this shell's environment would be one every case had asked for.
unset PUBKEY_SSH_ROOT PUBKEY_SSH_FIRST_USER TAILSCALE_KEYED

KEY='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGxHq5m1 test@example'
# pi-gen always creates a first user, `pi` unless the toml names one; with a
# key it gets a shell, without one it keeps `--disabled-login`'s.
U=${FIRST_USER:-pi}
if [ "$FIRST_USER_KEY" = 1 ]; then USHELL=/bin/bash; else USHELL=/usr/sbin/nologin; fi

# The tree every check passes on, for this product's knobs.
good() {
	T=$1
	mkdir -p "$T/etc/ssh/sshd_config.d" "$T/etc/systemd/system/multi-user.target.wants" \
		"$T/etc/systemd/system/sysinit.target.wants" "$T/usr/lib/systemd/system" \
		"$T/usr/lib/systemd/journald.conf.d" "$T/usr/lib/aarch64-linux-gnu" \
		"$T/boot/firmware" "$T/var/lib/NetworkManager" "$T/var/lib/misc" \
		"$T/usr/bin" "$T/usr/libexec" "$T/etc/NetworkManager/conf.d" "$T/etc/sudoers.d" \
		"$T/root/.ssh" "$T/home/$U/.ssh" "$T$CONFIG_DIR" "$T$STATE_DIR"

	printf 'Include /etc/ssh/sshd_config.d/*.conf\nSubsystem sftp /usr/lib/openssh/sftp-server\n' > "$T/etc/ssh/sshd_config"
	cp "$STAGE/00-appliance/files/sshd.conf" "$T/etc/ssh/sshd_config.d/00-$NAME.conf"
	printf 'dtparam=audio=on\n' > "$T/boot/firmware/config.txt"
	for unit in wpa_supplicant bluetooth regenerate_ssh_host_keys ssh "$NAME-firstboot"; do
		: > "$T/usr/lib/systemd/system/$unit.service"
	done
	cp "$SCAFFOLD/targets/image/systemd/$NAME.service" "$T/usr/lib/systemd/system/"
	ln -s /usr/lib/systemd/system/regenerate_ssh_host_keys.service "$T/etc/systemd/system/sysinit.target.wants/"
	for unit in ssh "$NAME" "$NAME-firstboot"; do
		ln -s "/usr/lib/systemd/system/$unit.service" "$T/etc/systemd/system/multi-user.target.wants/"
	done
	printf 'root:x:0:0:root:/root:/bin/bash\n%s:x:1000:1000::/home/%s:%s\n' "$U" "$U" "$USHELL" > "$T/etc/passwd"
	printf 'root:*:20000:0:99999:7:::\n%s:!:20000:0:99999:7:::\n' "$U" > "$T/etc/shadow"
	printf 'root:x:0:\nsudo:x:27:%s\n%s:x:1000:\n%s:x:999:\n' "$U" "$U" "$GROUP" > "$T/etc/group"
	# The distribution's own drop-in, which the package's has to beat.
	printf '[Journal]\nStorage=volatile\n' > "$T/usr/lib/systemd/journald.conf.d/40-rpi-volatile-storage.conf"
	printf 'hosts: files mdns4_minimal [NOTFOUND=return] dns\n' > "$T/etc/nsswitch.conf"
	: > "$T/usr/lib/aarch64-linux-gnu/libnss_mdns4_minimal.so.2"
	printf '#!/bin/sh\n' > "$T/usr/bin/$BINARY"
	printf '#!/bin/sh\n' > "$T/usr/libexec/$NAME-firstboot"
	chmod 0755 "$T/usr/bin/$BINARY" "$T/usr/libexec/$NAME-firstboot"
	printf '%s\n' "$HOSTNAME_PREFIX" > "$T/etc/hostname"
	mkdir -p "$T/etc/issue.d" "$T/etc/NetworkManager/dispatcher.d"
	printf 'Debian GNU/Linux 13 \\n \\l\n\n' > "$T/etc/issue"
	cp "$STAGE/00-appliance/files/console.issue" "$T/etc/issue.d/$NAME.issue"
	ln -s "/run/$NAME-addresses.issue" "$T/etc/issue.d/${NAME}_addresses.issue"
	install -m 0755 "$STAGE/00-appliance/files/console-addresses.sh" "$T/usr/libexec/$NAME-console-addresses"
	install -m 0755 "$STAGE/00-appliance/files/console-hostname.sh" "$T/etc/NetworkManager/dispatcher.d/90-$NAME-console"
	cp "$STAGE/00-appliance/files/console.service" "$T/usr/lib/systemd/system/$NAME-console.service"
	ln -s "/usr/lib/systemd/system/$NAME-console.service" "$T/etc/systemd/system/multi-user.target.wants/"

	if [ "$WIFI" = 0 ]; then wireless=false; else wireless=true; fi
	printf '[main]\nNetworkingEnabled=true\nWirelessEnabled=%s\nWWANEnabled=true\n' "$wireless" > "$T/var/lib/NetworkManager/NetworkManager.state"
	[ "$BLUETOOTH" = 1 ] || ln -s /dev/null "$T/etc/systemd/system/bluetooth.service"
	[ "$LINK_LOCAL" = 0 ] || cp "$STAGE/00-appliance/files/link-local.conf" "$T/etc/NetworkManager/conf.d/10-$NAME-link-local.conf"
	[ "$JOURNAL_PERSISTENT" = 0 ] || cp "$SCAFFOLD/targets/image/debian/journald.conf" "$T$JOURNALD_DROPIN"
	[ "$HOST_ADMIN" = 0 ] || printf 'AuthorizedKeysFile .ssh/authorized_keys .ssh/authorized_keys2 %s/ssh/%%u\n' "$CONFIG_DIR" > "$T/etc/ssh/sshd_config.d/50-$NAME.conf"
	if [ "$HATS" = 1 ]; then
		mkdir -p "$T${BOOT_FIRMWARE_DROPIN%/*}"
		printf '[Service]\nReadWritePaths=-/boot/firmware\n' > "$T$BOOT_FIRMWARE_DROPIN"
	fi
	[ "$ROOT_KEY" = 0 ] || printf '%s\n' "$KEY" > "$T/root/.ssh/authorized_keys"
	[ "$FIRST_USER_KEY" = 0 ] || printf '%s\n' "$KEY" > "$T/home/$U/.ssh/authorized_keys"
	if [ "$PASSWORDLESS_SUDO" = 1 ]; then
		printf '%s ALL=(ALL) NOPASSWD: ALL\n' "$U" > "$T/etc/sudoers.d/010_pi-nopasswd"
		chmod 0440 "$T/etc/sudoers.d/010_pi-nopasswd"
	fi
	if [ "$TAILSCALE" = 1 ]; then
		mkdir -p "$T/usr/sbin" "$T/var/lib/tailscale" "$T/etc/systemd/system/tailscaled.service.d"
		cp "$STAGE/00-appliance/files/tailscaled.conf" "$T/etc/systemd/system/tailscaled.service.d/$NAME.conf"
		printf '#!/bin/sh\n' > "$T/usr/bin/tailscale"
		printf '#!/bin/sh\n' > "$T/usr/sbin/tailscaled"
		chmod 0755 "$T/usr/bin/tailscale" "$T/usr/sbin/tailscaled"
		: > "$T/usr/lib/systemd/system/tailscaled.service"
		ln -s /usr/lib/systemd/system/tailscaled.service "$T/etc/systemd/system/multi-user.target.wants/"
		cp "$STAGE/00-appliance/files/tailscale-enrol.service" "$T/usr/lib/systemd/system/$NAME-tailscale-enrol.service"
		install -m 0755 "$STAGE/00-appliance/files/tailscale-enrol.sh" "$T/usr/libexec/$NAME-tailscale-enrol"
		ln -s "/usr/lib/systemd/system/$NAME-tailscale-enrol.service" "$T/etc/systemd/system/multi-user.target.wants/"
		if [ "$HOST_ADMIN" = 1 ]; then
			mkdir -p "$T/etc/systemd/system/$NAME.service.d"
			printf '[Service]\nReadWritePaths=/var/lib/misc -/var/lib/tailscale\n' > "$T/etc/systemd/system/$NAME.service.d/tailscale.conf"
		fi
	fi

	# Each file's `fixture` runs in a subshell of its own, as its `check` and
	# `cases` do, and one that fails stops the harness naming the file. A file
	# missing `fixture` is skipped here: verify.sh is what refuses it.
	for file in "$CHECKS"/*.sh; do
		[ -f "$file" ] || continue
		set +e
		(set -e; unset -f fixture; . "$file"; ! command -v fixture >/dev/null 2>&1 || fixture)
		status=$?
		set -e
		[ "$status" = 0 ] || { echo "run-verify-tests: ${file##*/}'s fixture exited $status" >&2; exit 2; }
	done
}

# `sed -i` differs between GNU and BSD; a mutation edits a file in place with this.
resed() { sed "$1" "$2" > "$2.new" && mv "$2.new" "$2"; }
key_in() { mkdir -p "${1%/*}" && printf '%s\n' "$KEY" > "$1"; }
# config.local's key on the first user, the shell 00-appliance then gives it, and no sudo.
dev_user() { key_in "home/$U/.ssh/authorized_keys" && resed 's|:/usr/sbin/nologin$|:/bin/bash|' etc/passwd && rm -f etc/sudoers.d/*; }
# What 00-appliance does with config.local's fleet key.
TKEY="var/lib/misc/$NAME-tailscale.key"
fleet_key() { (umask 077 && printf 'not-a-real-key\n' > "$TKEY"); }
enrol_off() { rm -f "etc/systemd/system/multi-user.target.wants/$NAME-tailscale-enrol.service"; }
# A group of this user's other than the tree's, for a key owned by the wrong one.
OTHER_GID=$(id -G | tr ' ' '\n' | grep -vx "$(ls -lnd "$TMP" | awk '{ print $4 }')" | head -n 1 || true)

# What config.local would have put in pi-gen's environment for one case:
# `with_env 'PUBKEY_SSH_ROOT=1' case_run …`.
CASE_ENV=""
with_env() { CASE_ENV=$1; shift; "$@"; CASE_ENV=""; }

# `case_run` also tallies on fd 9, read back from each product file's `cases`
# subshell; opened here so the chassis's own cases have somewhere to write it.
exec 9>/dev/null
pass=0
fail=0

# A fresh good tree, broken by the mutation, must fail with exactly `want`: an
# id, a space-separated set of ids, or empty for a mutation that must not fail.
case_run() {
	want=$1; label=$2; shift 2
	T="$TMP/case"; rm -rf "$T"; good "$T"
	(cd "$T" && eval "$@")
	status=0; out=$(env $CASE_ENV "$VERIFY" "$T" "$CHECKS" 2>&1) || status=$?
	got=$(printf '%s\n' "$out" | awk '/ FAIL: / { print $1 }' | sort -u | tr '\n' ' '); got=${got% }
	[ "$status" = 0 ] || [ -n "$got" ] || got="exit $status"
	[ "$status" != 0 ] || [ -z "$got" ] || got="$got, but exit 0"
	if [ "$got" = "$want" ]; then
		echo "PASS  ${want:+$want }$label"; echo pass >&9; pass=$((pass + 1))
	else
		echo "FAIL  ${want:+$want }$label (expected ${want:-no failure}, got: ${got:-none})"; echo fail >&9; fail=$((fail + 1))
		printf '%s\n' "$out"
	fi
}
case_pass() { case_run "" "$@"; }

case_pass "the good tree passes every check" ':'

case_run VERIFY-01 "sshd_config has no Include" \
	"resed '/^Include/d' etc/ssh/sshd_config"
case_run VERIFY-01 "a directive before the Include, which every drop-in then loses to" \
	"printf 'PasswordAuthentication yes\n' | cat - etc/ssh/sshd_config > c && mv c etc/ssh/sshd_config"
# Removing the directory falsifies the include target and every drop-in in it,
# and each failure is correct; pinned as a set because a check that stopped
# firing here would be a regression.
if [ "$HOST_ADMIN" = 1 ]; then dropins="VERIFY-01 VERIFY-02 VERIFY-03"; else dropins="VERIFY-01 VERIFY-02"; fi
case_run "$dropins" "sshd_config.d missing (trips every drop-in check, correctly)" \
	'rm -rf etc/ssh/sshd_config.d'

case_run VERIFY-02 "the password policy drop-in is missing" \
	"rm -f etc/ssh/sshd_config.d/00-$NAME.conf"
case_run VERIFY-02 "the drop-in sets only one of the two policies" \
	"printf 'PasswordAuthentication no\n' > etc/ssh/sshd_config.d/00-$NAME.conf"
# `.` sorts before every character a name may start with, so this file always
# sorts first — the position a distribution file could take.
case_run VERIFY-02 "a drop-in sorting first says PasswordAuthentication yes" \
	"printf 'PasswordAuthentication yes\n' > etc/ssh/sshd_config.d/00-.conf"
case_run VERIFY-02 "a drop-in sorting first says it with an equals sign" \
	"printf 'PasswordAuthentication=yes\n' > etc/ssh/sshd_config.d/00-.conf"
case_run VERIFY-02 "a Match block, in a drop-in sorting after ours" \
	"printf 'Match Address 0.0.0.0/0\n\tPasswordAuthentication yes\n' > etc/ssh/sshd_config.d/99-late.conf"
case_run VERIFY-02 "a Match block in sshd_config itself, spelled in lower case" \
	"printf 'match User support\n\tPermitRootLogin yes\n' >> etc/ssh/sshd_config"
case_pass "VERIFY-02 lets a drop-in sorting first agree with the policy" \
	"printf 'PasswordAuthentication no\n' > etc/ssh/sshd_config.d/00-.conf"

if [ "$HOST_ADMIN" = 1 ]; then
	case_run VERIFY-03 "the package's AuthorizedKeysFile drop-in is missing" \
		"rm -f etc/ssh/sshd_config.d/50-$NAME.conf"
	case_run VERIFY-03 "the drop-in does not name $CONFIG_DIR/ssh/%u" \
		"printf 'AuthorizedKeysFile .ssh/authorized_keys\n' > etc/ssh/sshd_config.d/50-$NAME.conf"
	case_run VERIFY-03 "an earlier drop-in sets AuthorizedKeysFile elsewhere" \
		"printf 'AuthorizedKeysFile .ssh/authorized_keys\n' > etc/ssh/sshd_config.d/10-hardening.conf"
	case_pass "VERIFY-03 lets an earlier drop-in name the same files" \
		"cp etc/ssh/sshd_config.d/50-$NAME.conf etc/ssh/sshd_config.d/10-same.conf"

	case_run VERIFY-04 "dtoverlay=disable-wifi" \
		'printf "dtoverlay=disable-wifi\n" >> boot/firmware/config.txt'
	case_run VERIFY-04 "dtoverlay=disable-bt" \
		'printf "dtoverlay=disable-bt\n" >> boot/firmware/config.txt'
	case_run VERIFY-04 "config.txt at the wrong path" \
		'rm -f boot/firmware/config.txt'

	case_run VERIFY-05 "wpa_supplicant.service masked" \
		'ln -s /dev/null etc/systemd/system/wpa_supplicant.service'
fi

if [ "$WIFI" = 0 ]; then
	case_run VERIFY-06 "Wi-Fi left on" \
		'printf "[main]\nNetworkingEnabled=true\nWirelessEnabled=true\nWWANEnabled=true\n" > var/lib/NetworkManager/NetworkManager.state'
	case_run VERIFY-06 "the state file absent" \
		'rm -f var/lib/NetworkManager/NetworkManager.state'
else
	case_run VERIFY-06 "Wi-Fi off on a product that ships it on" \
		'printf "[main]\nNetworkingEnabled=true\nWirelessEnabled=false\nWWANEnabled=true\n" > var/lib/NetworkManager/NetworkManager.state'
fi

if [ "$BLUETOOTH" = 0 ]; then
	case_run VERIFY-07 "bluetooth.service not masked" \
		'rm -f etc/systemd/system/bluetooth.service'
	case_run VERIFY-07 "bluetooth.service merely disabled, not masked" \
		'rm -f etc/systemd/system/bluetooth.service && printf "[Unit]\n" > etc/systemd/system/bluetooth.service'
else
	case_run VERIFY-07 "bluetooth.service masked on a product that ships it on" \
		'ln -s /dev/null etc/systemd/system/bluetooth.service'
fi

case_run VERIFY-08 "a host key shipped in the image" \
	'printf "PRIVATE KEY\n" > etc/ssh/ssh_host_ed25519_key'

case_run VERIFY-09 "regenerate_ssh_host_keys not installed" \
	'rm -f usr/lib/systemd/system/regenerate_ssh_host_keys.service'
case_run VERIFY-09 "regenerate_ssh_host_keys installed but no target wants it" \
	'rm -f etc/systemd/system/*.target.wants/regenerate_ssh_host_keys.service'

case_run VERIFY-10 "a password baked into the image" \
	"resed 's|^$U:!:|$U:\$6\$abcd\$hashhashhash:|' etc/shadow"
case_run VERIFY-10 "passwd -l left the hash behind the bang" \
	"resed 's|^$U:!:|$U:!\$6\$abcd\$hashhashhash:|' etc/shadow"
case_run VERIFY-10 "an empty password, which the console accepts" \
	"resed 's|^$U:!:|$U::|' etc/shadow"
case_run VERIFY-10 "a DES hash, which has no dollar sign" \
	"resed 's|^$U:!:|$U:ab3JfXtWd/8pM:|' etc/shadow"

case_run VERIFY-11 "a key on an account nobody named" \
	"printf 'guest:x:1001:1001::/home/guest:/bin/bash\n' >> etc/passwd && key_in home/guest/.ssh/authorized_keys"
case_run VERIFY-11 "a key in /etc/skel, which every account made later inherits" \
	'key_in etc/skel/.ssh/authorized_keys'
if [ "$ROOT_KEY" = 1 ]; then
	case_run VERIFY-11 "the root key appliance.toml asked for is missing" \
		'rm -f root/.ssh/authorized_keys'
	case_run VERIFY-11 "a second root key file nobody asked for" \
		'key_in root/.ssh/authorized_keys2'
	case_run VERIFY-11 "a root key with ssh.service not enabled - no way in" \
		'rm -f etc/systemd/system/multi-user.target.wants/ssh.service'
else
	case_run VERIFY-11 "a root key nobody asked for" \
		'key_in root/.ssh/authorized_keys'
	with_env PUBKEY_SSH_ROOT=1 case_pass "VERIFY-11 admits the root key config.local set" \
		'key_in root/.ssh/authorized_keys'
	with_env PUBKEY_SSH_ROOT=1 case_run VERIFY-11 "config.local's root key with ssh.service not enabled - no way in" \
		'key_in root/.ssh/authorized_keys && rm -f etc/systemd/system/multi-user.target.wants/ssh.service'
fi
if [ "$FIRST_USER_KEY" = 1 ]; then
	case_run VERIFY-11 "the first user's key appliance.toml asked for is missing" \
		"rm -f home/$U/.ssh/authorized_keys"
	case_run VERIFY-11 "the first user has a key and no login shell" \
		"resed 's|:/bin/bash\$|:/usr/sbin/nologin|' etc/passwd"
else
	case_run VERIFY-11 "a key on the first user nobody asked for" \
		"key_in home/$U/.ssh/authorized_keys"
	# A key config.local put on the first user, which the toml never saw.
	with_env PUBKEY_SSH_FIRST_USER=1 case_run VERIFY-11 "config.local's key on a first user whose shell is still nologin" \
		"key_in home/$U/.ssh/authorized_keys"
	with_env PUBKEY_SSH_FIRST_USER=1 case_run VERIFY-11 "config.local's key on a first user with a shell and no sudo - signs in, does nothing" \
		dev_user
	with_env PUBKEY_SSH_FIRST_USER=1 case_pass "VERIFY-11 accepts a first user with a key, a shell and passwordless sudo from config.local" \
		"dev_user && printf '%s ALL=(ALL) NOPASSWD: ALL\n' '$U' > etc/sudoers.d/010_pi-nopasswd"
	with_env PUBKEY_SSH_FIRST_USER=1 case_pass "VERIFY-11 accepts a first user with a key and a shell in the $GROUP group, which opens the socket" \
		"dev_user && resed 's|^$GROUP:x:999:\$|$GROUP:x:999:$U|' etc/group"
	with_env "PUBKEY_SSH_FIRST_USER=1 PUBKEY_SSH_ROOT=1" case_pass "VERIFY-11 accepts a first user with a key and no sudo beside a root key" \
		'dev_user && key_in root/.ssh/authorized_keys'
fi
if [ "$PASSWORDLESS_SUDO" = 1 ]; then
	case_run VERIFY-11 "the sudo grant appliance.toml asked for is missing" \
		'rm -f etc/sudoers.d/010_pi-nopasswd'
	# rm first: the file is 0440 and cannot be overwritten in place.
	case_run VERIFY-11 "a sudoers drop-in that still asks for a password" \
		"rm -f etc/sudoers.d/010_pi-nopasswd && printf '$U ALL=(ALL) ALL\n' > etc/sudoers.d/010_pi-nopasswd"
	case_run VERIFY-11 "a sudoers file sudo ignores, because its name has a dot" \
		"mv etc/sudoers.d/010_pi-nopasswd etc/sudoers.d/010_pi.nopasswd"
fi

if [ "$JOURNAL_PERSISTENT" = 1 ]; then
	case_run VERIFY-12 "the package's journald drop-in is missing" \
		"rm -f .$JOURNALD_DROPIN"
	case_run VERIFY-12 "persistent, but with no size cap" \
		"printf '[Journal]\nStorage=persistent\n' > .$JOURNALD_DROPIN"
	# The one a grep of our own file is blind to: a drop-in that sorts after
	# ours and wins.
	case_run VERIFY-12 "a later drop-in sets Storage=volatile" \
		"printf '[Journal]\nStorage=volatile\n' > usr/lib/systemd/journald.conf.d/99-late.conf"
	case_run VERIFY-12 "an /etc drop-in of the same name masks ours with volatile" \
		"mkdir -p etc/systemd/journald.conf.d && printf '[Journal]\nStorage=volatile\n' > etc/systemd/journald.conf.d/${JOURNALD_DROPIN##*/}"
	case_run VERIFY-12 "an /etc drop-in of the same name that says nothing masks ours entirely" \
		"mkdir -p etc/systemd/journald.conf.d && printf '[Journal]\n' > etc/systemd/journald.conf.d/${JOURNALD_DROPIN##*/}"
	case_run VERIFY-12 "ours masked with a /dev/null symlink under /etc" \
		"mkdir -p etc/systemd/journald.conf.d && ln -s /dev/null etc/systemd/journald.conf.d/${JOURNALD_DROPIN##*/}"
fi

if [ "$LINK_LOCAL" = 1 ]; then
	case_run VERIFY-13 "the link-local default is missing" \
		"rm -f etc/NetworkManager/conf.d/10-$NAME-link-local.conf"
	case_run VERIFY-13 "the link-local default says something else" \
		"printf '[connection]\nipv4.link-local=1\n' > etc/NetworkManager/conf.d/10-$NAME-link-local.conf"
	if [ "$RELEASE" = bookworm ]; then other=4; else other=3; fi
	case_run VERIFY-13 "the link-local default another release takes ($other)" \
		"printf '[connection]\nipv4.link-local=$other\n' > etc/NetworkManager/conf.d/10-$NAME-link-local.conf"
else
	case_run VERIFY-13 "a link-local default on a product that turned it off" \
		"printf '[connection]\nipv4.link-local=3\n' > etc/NetworkManager/conf.d/10-$NAME-link-local.conf"
fi

case_run VERIFY-14 "the binary is missing" \
	"rm -f usr/bin/$BINARY"
case_run VERIFY-14 "the unit is not enabled" \
	"rm -f etc/systemd/system/multi-user.target.wants/$NAME.service"
case_run VERIFY-14 "the first-boot script is missing" \
	"rm -f usr/libexec/$NAME-firstboot"
case_run VERIFY-14 "the first-boot unit is not enabled" \
	"rm -f etc/systemd/system/multi-user.target.wants/$NAME-firstboot.service"
case_run VERIFY-14 "a whole unit under /etc replaces ours" \
	"printf '[Service]\nExecStart=/usr/bin/$BINARY\n' > etc/systemd/system/$NAME.service"
case_run VERIFY-14 "ours masked with a /dev/null symlink under /etc" \
	"ln -s /dev/null etc/systemd/system/$NAME.service"
case_run VERIFY-14 "the first-boot unit masked under /etc" \
	"ln -s /dev/null etc/systemd/system/$NAME-firstboot.service"
case_run VERIFY-14 "a whole unit under /usr/local replaces ours" \
	"mkdir -p usr/local/lib/systemd/system && printf '[Service]\nExecStart=/usr/bin/$BINARY\n' > usr/local/lib/systemd/system/$NAME.service"
case_run VERIFY-14 "ours masked under /etc/systemd/system.control" \
	"mkdir -p etc/systemd/system.control && ln -s /dev/null etc/systemd/system.control/$NAME.service"
case_run VERIFY-14 "a whole unit under /etc/systemd/system.attached replaces ours" \
	"mkdir -p etc/systemd/system.attached && printf '[Service]\nExecStart=/usr/bin/$BINARY\n' > etc/systemd/system.attached/$NAME.service"

case_run VERIFY-15 "the first-boot marker was written on the build host" \
	"printf '%s-abc123\n' '$HOSTNAME_PREFIX' > var/lib/misc/$NAME-firstboot.done"
case_run VERIFY-15 "the box was named on the build host" \
	"printf '%s-abc123\n' '$HOSTNAME_PREFIX' > etc/hostname"
case_run VERIFY-15 "a serving pair was minted on the build host" \
	"printf 'KEY\n' > .$STATE_DIR/key.pem"
case_run VERIFY-15 "a database was made on the build host" \
	"printf 'SQLite\n' > .$STATE_DIR/$NAME.db"
case_run VERIFY-15 "a key was put where the daemon keeps the ones it manages" \
	"key_in .$CONFIG_DIR/ssh/root"

case_run VERIFY-16 "libnss-mdns installed but nsswitch does not name it" \
	"printf 'hosts: files dns\n' > etc/nsswitch.conf"

case_run VERIFY-17 "a getty autologin drop-in" \
	"mkdir -p etc/systemd/system/getty@tty1.service.d && printf '[Service]\nExecStart=\nExecStart=-/sbin/agetty --autologin $U --noclear %%I \$TERM\n' > etc/systemd/system/getty@tty1.service.d/autologin.conf"
case_run VERIFY-17 "the account wizard enabled" \
	"ln -s /usr/lib/systemd/system/userconfig.service etc/systemd/system/multi-user.target.wants/"
case_run VERIFY-17 "cloud-init installed" \
	"printf '#!/bin/sh\n' > usr/bin/cloud-init && chmod 0755 usr/bin/cloud-init"

UNIT=usr/lib/systemd/system/$NAME.service
case_run "VERIFY-14 VERIFY-18 VERIFY-22" "the unit is missing (trips every unit check, correctly)" \
	"rm -f $UNIT"
case_run VERIFY-22 "the unit predates the deployment lines" \
	"resed '/_ONBOARDING=/d' $UNIT"
if [ "$HOST_ADMIN" = 1 ]; then
	case_run VERIFY-22 "a host-admin unit that says unprivileged" \
		"resed 's/_PRIVILEGE=host-admin/_PRIVILEGE=unprivileged/' $UNIT"
fi
case_run VERIFY-18 "the unit has no After=time-sync.target" \
	"resed '/^After=time-sync/d' $UNIT"
case_run VERIFY-18 "the unit says Wants=time-sync.target and no After=" \
	"resed 's/^After=time-sync/Wants=time-sync/' $UNIT"
case_run VERIFY-18 "the ordering commented out" \
	"resed 's/^After=time-sync/#After=time-sync/' $UNIT"
case_run VERIFY-18 "the ordering moved to [Install], where it orders nothing" \
	"resed '/^After=time-sync/d' $UNIT && printf 'After=time-sync.target\n' >> $UNIT"

case_run VERIFY-23 "apt marks the package automatically installed" \
	"mkdir -p var/lib/apt && printf 'Package: libfoo\nAuto-Installed: 0\n\nPackage: $PACKAGE\nArchitecture: arm64\nAuto-Installed: 1\n' > var/lib/apt/extended_states"
case_pass "VERIFY-23 lets apt mark other packages, and this one manual" \
	"mkdir -p var/lib/apt && printf 'Package: $PACKAGE-dbg\nAuto-Installed: 1\n\nPackage: $PACKAGE\nArchitecture: arm64\nAuto-Installed: 0\n' > var/lib/apt/extended_states"

ISSUE="etc/issue.d/$NAME.issue"
# The console screen rewritten by a filter: `issue_edit grep -v …`.
issue_edit() { "$@" "$ISSUE" > "$ISSUE.new" && mv "$ISSUE.new" "$ISSUE"; }
case_run VERIFY-24 "/etc/issue is missing, so agetty skips issue.d" \
	'rm -f etc/issue'
case_run VERIFY-24 "the console screen is missing" \
	'rm -f "$ISSUE"'
case_run VERIFY-24 "the console screen does not name the box" \
	'issue_edit grep -vF "https://\n.local"'
if [ "$LOCAL_ALIAS" = 1 ]; then
	case_run VERIFY-24 "the console screen does not name the alias" \
		'issue_edit grep -vF "https://$HOSTNAME_PREFIX.local"'
else
	case_run VERIFY-24 "the console screen names an alias nobody publishes" \
		'printf "  https://%s.local\n" "$HOSTNAME_PREFIX" >> "$ISSUE"'
fi
if [ "$CONSOLE_ART" = 1 ]; then
	case_run VERIFY-24 "the console screen lost its art" \
		"issue_edit awk 'shown || /^Open in a browser/ { shown = 1; print }'"
	case_run VERIFY-24 "a single backslash in the art, before an n" \
		"issue_edit awk 'NR == 1 { print \"a \\\\n b\" } { print }'"
	case_pass "VERIFY-24 accepts a doubled backslash in the art, which agetty prints as one" \
		"issue_edit awk 'NR == 1 { print \"a \\\\\\\\n b\" } { print }'"
else
	case_run VERIFY-24 "art above the console screen nobody asked for" \
		"issue_edit awk 'NR == 1 { print \"( o.o )\" } { print }'"
fi
DISPATCHER="etc/NetworkManager/dispatcher.d/90-$NAME-console"
case_run VERIFY-25 "Raspberry Pi OS's IP.issue, which prints the first address of any interface" \
	"printf 'My IP address is \\\\4 \\\\6\n\n' > etc/issue.d/IP.issue"
case_run VERIFY-25 "an address agetty picks from a named interface" \
	"printf 'eth0 \\\\4{eth0}\n' > etc/issue.d/zz-more.issue"
case_pass "VERIFY-25 accepts a doubled backslash before a 4, which prints as text" \
	"printf '\\\\\\\\4 is a digit\n' > etc/issue.d/zz-more.issue"
case_run VERIFY-25 "the address lines are not linked into issue.d" \
	"rm -f etc/issue.d/${NAME}_addresses.issue"
case_run VERIFY-25 "the address lines are linked from somewhere else" \
	"ln -sfn /run/elsewhere.issue etc/issue.d/${NAME}_addresses.issue"
case_run VERIFY-25 "the rename hook is missing from dispatcher.d" \
	'rm -f "$DISPATCHER"'
case_run VERIFY-25 "the rename hook is writable by its group, so NetworkManager skips it" \
	'chmod 0775 "$DISPATCHER"'
case_run VERIFY-25 "the rename hook is not executable" \
	'chmod 0644 "$DISPATCHER"'
# Only root can hand a file to another owner.
if [ "$(id -u)" = 0 ]; then
	case_run VERIFY-25 "the rename hook is not root's" \
		'chown 1 "$DISPATCHER"'
fi
case_run VERIFY-25 "the address watcher's unit is missing beside its wants link" \
	"rm -f usr/lib/systemd/system/$NAME-console.service"
case_run VERIFY-25 "the address watcher is not enabled" \
	"rm -f etc/systemd/system/multi-user.target.wants/$NAME-console.service"
case_run VERIFY-25 "the address script is missing" \
	"rm -f usr/libexec/$NAME-console-addresses"

case_run VERIFY-26 "config.txt names HATs before any card has booted" \
	"printf '# BEGIN %s HATs\n[all]\n# rtc\ndtoverlay=i2c-rtc,ds3231\n# END %s HATs\n' '$NAME' '$NAME' >> boot/firmware/config.txt"
case_pass "VERIFY-26 leaves an overlay a product substage added outside the block" \
	"printf 'dtoverlay=i2c-rtc,ds3231\n' >> boot/firmware/config.txt"
if [ "$HATS" = 1 ]; then
	case_run VERIFY-26 "the daemon's sandbox is not opened to the boot partition" \
		"rm -f .$BOOT_FIRMWARE_DROPIN"
	case_run VERIFY-26 "the drop-in opens it without the -, so a box without one would not start the daemon" \
		"printf '[Service]\nReadWritePaths=/boot/firmware\n' > .$BOOT_FIRMWARE_DROPIN"
else
	case_run VERIFY-26 "the boot partition is opened to a daemon without hats" \
		"mkdir -p .${BOOT_FIRMWARE_DROPIN%/*} && printf '[Service]\nReadWritePaths=-/boot/firmware\n' > .$BOOT_FIRMWARE_DROPIN"
fi

case_run VERIFY-15 "a Tailscale node was made on the build host" \
	"mkdir -p var/lib/tailscale && printf '{}\n' > var/lib/tailscale/tailscaled.state"

if [ "$TAILSCALE" = 1 ]; then
	case_run VERIFY-19 "tailscaled is not installed" \
		'rm -f usr/sbin/tailscaled'
	case_run VERIFY-19 "tailscaled.service is not enabled" \
		'rm -f etc/systemd/system/multi-user.target.wants/tailscaled.service'
	TS_D=etc/systemd/system/tailscaled.service.d
	case_run VERIFY-19 "no drop-in turning tailscaled's log upload off" \
		"rm -f $TS_D/$NAME.conf"
	case_run VERIFY-19 "the drop-in turns the log upload back on" \
		"printf '[Service]\nEnvironment=TS_NO_LOGS_NO_SUPPORT=false\n' > $TS_D/$NAME.conf"
	case_run VERIFY-19 "a later drop-in undoes ours" \
		"printf '[Service]\nEnvironment=TS_NO_LOGS_NO_SUPPORT=false\n' > $TS_D/zz-later.conf"
	case_run VERIFY-19 "tailscaled's EnvironmentFile undoes ours" \
		"mkdir -p etc/default && printf 'TS_NO_LOGS_NO_SUPPORT=false\n' > etc/default/tailscaled"
	for d in etc/systemd/system usr/lib/systemd/system; do
		case_run VERIFY-19 "a drop-in for every service in /$d/service.d undoes ours" \
			"mkdir -p $d/service.d && printf '[Service]\nEnvironment=TS_NO_LOGS_NO_SUPPORT=false\n' > $d/service.d/10-all.conf"
	done
	case_run VERIFY-19 "a whole tailscaled.service in /etc undoes ours" \
		"printf '[Service]\nEnvironment=TS_NO_LOGS_NO_SUPPORT=false\nExecStart=/usr/sbin/tailscaled\n' > etc/systemd/system/tailscaled.service"
	case_run VERIFY-18 "the enrolment is not ordered after time-sync.target" \
		"resed 's/ time-sync.target\$//' usr/lib/systemd/system/$NAME-tailscale-enrol.service"
	with_env TAILSCALE_KEYED=1 case_pass "VERIFY-20 accepts config.local's key, root's alone, with the enrolment enabled" \
		fleet_key
	with_env TAILSCALE_KEYED=1 case_run VERIFY-20 "config.local's key did not reach the image" \
		:
	with_env TAILSCALE_KEYED=1 case_run VERIFY-20 "the key is readable by its group" \
		"fleet_key && chmod 0640 $TKEY"
	if [ -n "$OTHER_GID" ]; then
		with_env TAILSCALE_KEYED=1 case_run VERIFY-20 "the key belongs to another group" \
			"fleet_key && chgrp $OTHER_GID $TKEY"
	fi
	with_env TAILSCALE_KEYED=1 case_run VERIFY-20 "the key is in, but nothing enrols with it" \
		"fleet_key && enrol_off"
	with_env TAILSCALE_KEYED=1 case_run VERIFY-20 "the enrolment script is missing" \
		"fleet_key && rm -f usr/libexec/$NAME-tailscale-enrol"
	case_run VERIFY-20 "no fleet key, and the enrolment is not enabled for a key given later" \
		enrol_off
	if [ "$HOST_ADMIN" = 1 ]; then
		case_run VERIFY-21 "the daemon's sandbox is not opened for Tailscale's controls" \
			"rm -f etc/systemd/system/$NAME.service.d/tailscale.conf"
		case_run VERIFY-21 "the sandbox drop-in leaves tailscaled's state read-only" \
			"printf '[Service]\nReadWritePaths=/var/lib/misc\n' > etc/systemd/system/$NAME.service.d/tailscale.conf"
		case_run VERIFY-21 "tailscaled's state directory is missing" \
			'rm -rf var/lib/tailscale'
	fi
else
	case_run VERIFY-19 "Tailscale installed on a product with no [targets.image.tailscale]" \
		"mkdir -p usr/sbin && printf '#!/bin/sh\n' > usr/sbin/tailscaled"
fi
case_run VERIFY-20 "a fleet key nobody asked for" \
	fleet_key

case_pass "VERIFY-10 accepts '!!' (never set) as no password" \
	"printf 'root:*:20000:0:99999:7:::\n$U:!!:20000:0:99999:7:::\n' > etc/shadow"
case_pass "VERIFY-16 leaves nsswitch alone where libnss-mdns is absent" \
	"rm -f usr/lib/*/libnss_mdns4_minimal.so.2 && printf 'hosts: files dns\n' > etc/nsswitch.conf"
case_pass "VERIFY-14 lets a drop-in under /etc change the unit" \
	"mkdir -p etc/systemd/system/$NAME.service.d && printf '[Service]\nEnvironment=RUST_LOG=debug\n' > etc/systemd/system/$NAME.service.d/local.conf"
case_pass "VERIFY-18 accepts the ordering on one line with another unit, spaced" \
	"resed '/^After=time-sync/d' $UNIT && resed 's/^After=network-online.target\$/After = network-online.target time-sync.target/' $UNIT"
if [ "$BLUETOOTH" = 0 ]; then
	case_pass "VERIFY-07 has nothing to mask where bluez is absent" \
		'rm -f usr/lib/systemd/system/bluetooth.service etc/systemd/system/bluetooth.service'
fi
if [ "$JOURNAL_PERSISTENT" = 1 ]; then
	case_pass "VERIFY-12 lets an operator's 99-local.conf keep persistent" \
		"mkdir -p etc/systemd/journald.conf.d && printf '[Journal]\nSystemMaxUse=32M\n' > etc/systemd/journald.conf.d/99-local.conf"
fi

# Each file's `cases` in a subshell, its results counted from its fd 9 tally,
# so nothing it assigns — `pass`, `fail` — reaches this shell.
product_cases() {
	for file in "$CHECKS"/*.sh; do
		[ -f "$file" ] || continue
		set +e
		(set -e; unset -f cases; . "$file"; ! command -v cases >/dev/null 2>&1 || cases; echo finished >&9) 9>"$TMP/tally"
		set -e
		read -r passed failed << EOF
$(awk '$0 == "pass" { p++ } $0 == "fail" { f++ } END { print p + 0, f + 0 }' "$TMP/tally")
EOF
		pass=$((pass + passed)); fail=$((fail + failed))
		if [ "$(tail -n 1 "$TMP/tally")" != finished ]; then
			echo "FAIL  ${file##*/} stopped before its cases finished"; fail=$((fail + 1))
		fi
	done
}
product_cases

# Last, because they swap the checks directory. A product check file missing
# one of its three functions, or stopping early, fails by its own name rather
# than passing in silence.
CHECKS="$TMP/checks.d"
BROKEN="$CHECKS/99-broken.sh"
mkdir -p "$CHECKS"
# A checks.d file from the bodies of its `check`, `fixture` and `cases`.
check_file() { printf 'check() { %s; }\nfixture() { %s; }\ncases() { %s; }\n' "$1" "$2" "$3"; }

for fn in check fixture cases; do
	check_file : : : | grep -v "^$fn" > "$BROKEN"
	case_run 99-broken.sh "a checks.d file with no $fn function is refused" ':'
done
for stop in 'exit 3' 'exit 0'; do
	check_file "$stop" : : > "$BROKEN"
	case_run 99-broken.sh "a check that stops with '$stop' fails by its file's name" ':'
done
check_file 'fail QUIET-01 silenced >/dev/null 2>&1' : : > "$BROKEN"
case_run "exit 1" "a failure a check sends to /dev/null still fails verify.sh" ':'
printf '#!/bin/sh\nsleep 10 &\n' > "$TMP/lingers.sh"
check_file "sh $TMP/lingers.sh >/dev/null 2>&1" : : > "$BROKEN"
started=$(date +%s)
case_pass "a process a check leaves running does not hold verify.sh" ':'
[ $(($(date +%s) - started)) -lt 5 ] || { echo "FAIL  verify.sh waited for a check's process"; fail=$((fail + 1)); }

# The harness's own arms: one file's `cases` must add exactly $2 passes and $3
# failures, which are then taken back and the arm itself counted.
cases_tally() {
	was_pass=$pass was_fail=$fail
	product_cases > "$TMP/cases_tally.out" 2>&1
	got="$((pass - was_pass))/$((fail - was_fail))"
	pass=$was_pass fail=$was_fail
	if [ "$got" = "$2/$3" ]; then
		echo "PASS  $1"; pass=$((pass + 1))
	else
		echo "FAIL  $1 (expected $2/$3 passed/failed, got $got)"; fail=$((fail + 1))
		cat "$TMP/cases_tally.out"
	fi
}
check_file : : 'case_pass "a failing mutation" false' > "$BROKEN"
cases_tally "a mutation that fails stops the file's cases" 0 1
check_file : : 'false; case_pass "after a failed command" :' > "$BROKEN"
cases_tally "a command that fails before the first case stops the file's cases" 0 1
check_file : 'false; :' 'case_pass "on a fixture that failed" :' > "$BROKEN"
cases_tally "a fixture command that fails stops the file's cases" 0 1
check_file 'exit 3' : 'case_run 99-broken.sh "a failure in verify.sh'\''s own shell" :' > "$BROKEN"
cases_tally "verify.sh's own failures reach its exit status, not the harness's tally" 1 0
check_file : : 'exit 0' > "$BROKEN"
cases_tally "cases that stop with 'exit 0' fail by the file's name" 0 1
printf '#!/bin/sh\necho "  STUB-01 FAIL: printed"\n' > "$TMP/lying-verify.sh"
chmod +x "$TMP/lying-verify.sh"
check_file : : 'VERIFY=$TMP/lying-verify.sh; case_run STUB-01 "a failure with exit 0" :' > "$BROKEN"
cases_tally "a verify.sh that prints a failure and exits 0 fails the case" 0 1
{
	echo 'fails=0 fail=0 pass=0'
	check_file 'fail CLOBBER-01 always; fails=0' : 'case_run CLOBBER-01 "a check that zeroes fails" :; fail=0 pass=0'
} > "$BROKEN"
cases_tally "a checks.d file that zeroes every counter still fails, and leaves the harness's" 1 0
{ echo 'SHARED_BY_A=1'; check_file : : :; } > "$CHECKS/98-a.sh"
check_file '[ -z "${SHARED_BY_A:-}" ] || fail SHARED-01 "saw 98-a.sh'\''s variable"' \
	'[ -z "${SHARED_BY_A:-}" ]' 'case_pass "a file sees nothing another assigned" :' > "$BROKEN"
cases_tally "a file sees nothing another assigned, in check or fixture" 1 0

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
