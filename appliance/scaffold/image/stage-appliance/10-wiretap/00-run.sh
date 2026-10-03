#!/bin/bash -e
# Installs wiretap-server, which appliance/scripts/stage-wiretap-server.sh put
# in files/ before the build.
#
# Staged under /var/cache because on_chroot mounts a fresh tmpfs over /tmp and
# /run. apt-get rather than dpkg -i, so the package's Depends resolve and it is
# marked manually installed: export-image's autoremove takes back anything
# marked automatic.

DEB=files/wiretap-server.deb
[ -f "$DEB" ] || {
	echo "10-wiretap: no $DEB - run appliance/scripts/stage-wiretap-server.sh first" >&2
	exit 1
}

DEB_IN_ROOTFS=/var/cache/wiretap-server-install.deb
install -D -m 0644 "$DEB" "${ROOTFS_DIR}${DEB_IN_ROOTFS}"
on_chroot << EOF
apt-get install -y ${DEB_IN_ROOTFS}
EOF
rm -f "${ROOTFS_DIR}${DEB_IN_ROOTFS}"
