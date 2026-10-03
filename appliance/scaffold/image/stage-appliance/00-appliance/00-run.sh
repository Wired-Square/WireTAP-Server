#!/bin/bash -e
# Chassis-owned. Installs wiretap-appliance into the image, sets sshd's policy, the
# link-local default and the radios, installs Tailscale where appliance.toml
# asks for it, arranges first boot, and names libnss-mdns in nsswitch where its
# own postinst did not.
#
# **Everything here runs on the build host, in a chroot.** Nothing in this file
# may produce an identity: a key, a certificate or a name made here is one every
# card written from this image would share. Anything of that kind belongs in the
# first-boot unit installed below, which runs on the board.

# **`-D` on each, because `install` does not make parents without it** and
# `/usr/libexec` is not a directory a Debian rootfs is guaranteed to already
# have — Policy only started allowing it recently, so nothing has had a reason
# to create one.
install -D -m 0644 files/firstboot.service \
	"${ROOTFS_DIR}/usr/lib/systemd/system/wiretap-appliance-firstboot.service"
install -D -m 0755 files/firstboot.sh \
	"${ROOTFS_DIR}/usr/libexec/wiretap-appliance-firstboot"

# **No password over ssh, whichever key the card carries.** pi-gen's
# `PUBKEY_ONLY_SSH` sets `PasswordAuthentication no` only beside a first-user
# key, so a card with a root key alone keeps sshd's default of `yes`.
install -D -m 0644 files/sshd.conf \
	"${ROOTFS_DIR}/etc/ssh/sshd_config.d/00-wiretap-appliance.conf"

# A 169.254 address beside the lease, so a box on a bare cable still answers
# on `wiretap-appliance-<serial>.local`. A conf.d default rather than a keyfile: it
# applies to every connection, including the one an operator adds later.
install -D -m 0644 files/link-local.conf \
	"${ROOTFS_DIR}/etc/NetworkManager/conf.d/10-wiretap-appliance-link-local.conf"

# **Wi-Fi off, the way the Network screen turns it off.** pi-gen writes this
# file with WirelessEnabled=false only when WPA_COUNTRY is unset, and it is set
# for the regulatory domain — so without this NetworkManager defaults to on.
install -d -m 0755 "${ROOTFS_DIR}/var/lib/NetworkManager"
cat > "${ROOTFS_DIR}/var/lib/NetworkManager/NetworkManager.state" <<'STATE'
[main]
NetworkingEnabled=true
WirelessEnabled=false
WWANEnabled=true
STATE
chmod 0644 "${ROOTFS_DIR}/var/lib/NetworkManager/NetworkManager.state"

# **Bluetooth off, the way the Network screen turns it off: masked, not disabled.**
# bluetooth.service is bus-activated, so a client touching org.bluez — the
# daemon is one — would start a merely disabled unit. systemctl masks a unit
# that does not exist without complaint, hence the check that BlueZ is there;
# without it nothing powers the radio, and there is nothing to mask.
on_chroot << 'EOF'
if [ -f /usr/lib/systemd/system/bluetooth.service ]; then
	systemctl mask bluetooth.service
else
	echo "00-appliance: bluez is not installed, so bluetooth.service is not masked" >&2
fi
EOF

# **Tailscale, from its own repository.** Its postinst enables tailscaled only
# where it was already enabled, which on a fresh rootfs is nowhere. The
# drop-in, not /etc/default/tailscaled, which is a conffile.
install -D -m 0644 files/tailscale.sources \
	"${ROOTFS_DIR}/etc/apt/sources.list.d/tailscale.sources"
install -D -m 0644 files/tailscaled.conf \
	"${ROOTFS_DIR}/etc/systemd/system/tailscaled.service.d/wiretap-appliance.conf"
install -D -m 0644 files/tailscale-enrol.service \
	"${ROOTFS_DIR}/usr/lib/systemd/system/wiretap-appliance-tailscale-enrol.service"
install -D -m 0755 files/tailscale-enrol.sh \
	"${ROOTFS_DIR}/usr/libexec/wiretap-appliance-tailscale-enrol"
on_chroot << 'EOF'
apt-get update
apt-get install -y tailscale
systemctl enable tailscaled.service
EOF

# **The fleet key, root's alone**, outside every directory the appliance's
# group can read. The enrolment is enabled with or without one: its condition
# makes it a no-op until a key waits, and a key an Admin gives later is then
# retried at every boot until it joins.
TAILSCALE_KEY="${ROOTFS_DIR}/var/lib/misc/wiretap-appliance-tailscale.key"
if [ -f files/tailscale.key ]; then
	install -D -m 0600 -o root -g root files/tailscale.key "${TAILSCALE_KEY}"
else
	rm -f "${TAILSCALE_KEY}"
fi
on_chroot << 'EOF'
systemctl enable wiretap-appliance-tailscale-enrol.service
EOF

# **The daemon's sandbox, opened to the key's directory and tailscaled's
# state**, which switching Tailscale, an Admin's enrolment, leaving the
# tailnet and the factory reset write; `ProtectSystem=strict` leaves both
# read-only otherwise. The state directory is made now because one missing
# when the daemon starts stays read-only to it; the `-` lets the daemon start
# without it all the same.
install -d -m 0700 "${ROOTFS_DIR}/var/lib/tailscale"
install -d -m 0755 "${ROOTFS_DIR}/etc/systemd/system/wiretap-appliance.service.d"
cat > "${ROOTFS_DIR}/etc/systemd/system/wiretap-appliance.service.d/tailscale.conf" << 'EOF'
[Service]
ReadWritePaths=/var/lib/misc -/var/lib/tailscale
EOF

# **Not `/tmp`, and not `/run`.** `on_chroot` mounts a fresh tmpfs over both
# before it chroots, so a file staged there from the host is invisible to the
# commands below and `dpkg -i` fails with "cannot access archive". Nothing in
# pi-gen's own stages ever stages a file through either path, which is the tell.
DEB_IN_ROOTFS=/var/cache/wiretap-appliance-install.deb
install -D -m 0644 files/appliance.deb "${ROOTFS_DIR}${DEB_IN_ROOTFS}"

# **A real dpkg install, not an unpack.** The maintainer scripts are the whole
# point: postinst creates the account and the group, and gives /etc/wiretap-appliance the
# group ownership systemd will not. DPKG_ROOT is deliberately unset here — this
# *is* the target filesystem from inside the chroot — so postinst's
# deb-systemd-helper block runs and the unit is enabled in the image. Its
# systemctl block is skipped on its own, because a chroot has no
# /run/systemd/system.
#
# **The prose lives out here, not inside the heredoc.** The delimiter is
# unquoted so that ${DEB_IN_ROOTFS} expands on the host, which means the host
# shell also reads everything else in there — and a backtick in a comment is a
# command substitution it will run, as root, before the chroot ever sees it.
on_chroot << EOF
dpkg -i ${DEB_IN_ROOTFS}
systemctl enable wiretap-appliance-firstboot.service
EOF

# **A key without a password still gets a shell.** pi-gen creates the first user
# with `adduser --disabled-login`, which is `/usr/sbin/nologin` until a
# `FIRST_USER_PASS` says otherwise — so a card built with a key alone has sshd
# accept the key and then refuse the session. An appliance never ships a
# password, so the shell follows the key. `sudo` for the socket is pi-gen's own
# `PASSWORDLESS_SUDO=1`, applied in stage2.
if [ -n "${PUBKEY_SSH_FIRST_USER:-}" ] && [ -z "${FIRST_USER_PASS:-}" ]; then
	on_chroot << EOF
usermod --shell /bin/bash ${FIRST_USER_NAME}
EOF
fi

# **A key that signs in as root**, declared for export by the rendered `config`
# and set in `config.local`. Root already has a shell and no password, and
# sshd's Debian default of `PermitRootLogin prohibit-password` admits exactly
# this and nothing else — so a developer's card needs no second account and no
# sudo to reach the control socket.
if [ -n "${PUBKEY_SSH_ROOT:-}" ]; then
	install -d -m 0700 "${ROOTFS_DIR}/root/.ssh"
	printf '%s\n' "${PUBKEY_SSH_ROOT}" > "${ROOTFS_DIR}/root/.ssh/authorized_keys"
	chmod 0600 "${ROOTFS_DIR}/root/.ssh/authorized_keys"
fi

# **Removed from the host side.** A `rm` inside the heredoc would run against
# the chroot, which is the same filesystem — but doing it here is what makes it
# visible that the package must not survive into the image, and it still works
# if the chroot step is ever changed to a mount namespace of its own.
rm -f "${ROOTFS_DIR}${DEB_IN_ROOTFS}"

# **`.local` names resolve on the box too**, so a product can reach a peer by
# the name a laptop reaches this box by. libnss-mdns's own postinst rewrites the
# hosts line when it is installed; this fires only where it did not, and only
# where the module is there to be named. `mdns4_minimal`, not `mdns_minimal`:
# the IPv6-capable module stalls for seconds per lookup against an IPv4-only
# `.local` host.
NSS="${ROOTFS_DIR}/etc/nsswitch.conf"
if ls "${ROOTFS_DIR}"/usr/lib/*/libnss_mdns4_minimal.so.2 >/dev/null 2>&1 &&
	! grep -qE '^hosts:.*mdns' "${NSS}"; then
	echo "00-appliance: adding mdns4_minimal to nsswitch.conf (libnss-mdns did not)"
	sed -i -E 's/^(hosts:[[:space:]]+files)([[:space:]]+)/\1 mdns4_minimal [NOTFOUND=return]\2/' "${NSS}"
fi
