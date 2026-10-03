#!/bin/bash -e
# The local gateway, installed and left off. Its images are pulled on its
# first start, not baked in, and it refuses to start until storage is mounted
# at /srv/wiretap-gateway.

install -D -m 0644 files/compose.yaml \
	"${ROOTFS_DIR}/usr/share/wiretap-appliance/gateway/compose.yaml"
install -D -m 0644 files/gateway.env.example \
	"${ROOTFS_DIR}/usr/share/wiretap-appliance/gateway/gateway.env.example"
install -D -m 0644 files/wiretap-gateway.service \
	"${ROOTFS_DIR}/usr/lib/systemd/system/wiretap-gateway.service"
install -d -m 0755 "${ROOTFS_DIR}/srv/wiretap-gateway"
install -d -m 0700 "${ROOTFS_DIR}/etc/wiretap-gateway"

# dockerd and containerd start only when docker.socket is first used.
on_chroot << 'EOF'
systemctl disable docker.service containerd.service
EOF
