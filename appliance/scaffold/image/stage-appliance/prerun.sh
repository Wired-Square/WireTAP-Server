#!/bin/bash -e
# Chassis-owned. pi-gen runs this before the stage's substages.
#
# The standard pi-gen prerun: start this stage's root filesystem from the
# previous stage's, copying it once rather than rebuilding from stage0.
if [ ! -d "${ROOTFS_DIR}" ]; then
	copy_previous
fi
