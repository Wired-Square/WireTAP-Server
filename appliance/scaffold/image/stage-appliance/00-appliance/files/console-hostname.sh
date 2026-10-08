#!/bin/sh
# Chassis-owned. NetworkManager runs this on each dispatcher event; after a
# rename it has agetty reprint the console login screen, whose name is live.
[ "${2:-}" != hostname ] || agetty --reload 2>/dev/null
exit 0
