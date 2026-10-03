#!/bin/bash -e
# Chassis-owned. Fails the build on what the finished rootfs has to be true
# of, after every substage that sorts before this one — so a product's own
# substage is covered. A product's own checks are `checks.d/*.sh` beside this,
# which nothing here writes.

# **Checked against what sshd would apply, not against the file.** A drop-in
# that sorts before ours wins, so the file being in place proves nothing. `-G`
# rather than `-T`: it prints the parsed configuration before sshd loads host
# keys, which stage2 stripped, and before it looks for `/run/sshd`, which
# `on_chroot`'s fresh tmpfs lacks; it evaluates no Match block, which is why
# verify.sh refuses one. **The delimiter is quoted on purpose**: unquoted,
# `$(sshd -G)` runs on the host, and if that fails the chroot greps an empty
# string, which matches. The three `none`/`no` lines are ways in that no
# authorized_keys file would show.
# The AuthorizedKeysFile line is the package's: postinst wrote the drop-in and
# could only warn about it, because `sshd -t` wants the host keys first boot makes.
on_chroot << 'EOF'
effective=$(sshd -G) || {
	echo "90-verify: sshd -G failed (see above; -G needs OpenSSH 9.6 or later)" >&2
	exit 1
}
for policy in 'passwordauthentication no' 'kbdinteractiveauthentication no' \
	'authorizedkeyscommand none' 'trustedusercakeys none' 'hostbasedauthentication no' \
	'authorizedkeysfile .ssh/authorized_keys .ssh/authorized_keys2 /etc/wiretap-appliance/ssh/%u'; do
	printf '%s\n' "$effective" | grep -qxF "$policy" || {
		echo "90-verify: sshd would not apply '$policy' — a drop-in of the product's or the distribution's sets it otherwise" >&2
		exit 1
	}
done
EOF

# A root key is admitted by sshd's Debian default and by nothing stricter;
# `-G` spells that default `without-password` or `prohibit-password`,
# depending on the release.
if [ -n "${PUBKEY_SSH_ROOT:-}" ]; then
	on_chroot << 'EOF'
sshd -G | grep -qxE 'permitrootlogin (without-password|prohibit-password|yes)' || {
	echo "90-verify: a root key is installed but sshd would not admit root by key" >&2
	exit 1
}
EOF
fi

# config.local's fleet key reached the stage as this file, never as a variable.
[ ! -f ../00-appliance/files/tailscale.key ] || export TAILSCALE_KEYED=1

# Everything else is a file fact, and the same script the harness proves.
./verify.sh "${ROOTFS_DIR}" checks.d
