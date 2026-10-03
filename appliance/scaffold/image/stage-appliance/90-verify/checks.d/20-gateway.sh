# The local gateway 20-gateway installs: present, off, and kept off the SD card.

WIRETAP_GW_UNIT=wiretap-gateway.service
WIRETAP_GW_COMPOSE=usr/share/wiretap-appliance/gateway/compose.yaml
WIRETAP_GW_DATA=/srv/wiretap-gateway
WIRETAP_GW_DOCKER=usr/bin/docker
WIRETAP_GW_PLUGIN=usr/libexec/docker/cli-plugins/docker-compose

check() {
	wiretap_gw_unit="$R/usr/lib/systemd/system/$WIRETAP_GW_UNIT"
	wiretap_gw_links=$(find "$R/etc/systemd/system" -name "$WIRETAP_GW_UNIT" 2>/dev/null)
	if [ ! -f "$wiretap_gw_unit" ]; then
		fail WIRETAP-10 "$WIRETAP_GW_UNIT is not installed"
	elif [ -n "$wiretap_gw_links" ]; then
		fail WIRETAP-10 "$WIRETAP_GW_UNIT is enabled or masked in the image - the gateway ships off: ${wiretap_gw_links#"$R"}"
	else
		ok WIRETAP-10 "$WIRETAP_GW_UNIT is installed and disabled"
	fi

	if [ -f "$wiretap_gw_unit" ]; then
		if grep -qx "AssertPathIsMountPoint=$WIRETAP_GW_DATA" "$wiretap_gw_unit"; then
			ok WIRETAP-11 "$WIRETAP_GW_UNIT refuses to start without storage at $WIRETAP_GW_DATA"
		else
			fail WIRETAP-11 "$WIRETAP_GW_UNIT does not assert that $WIRETAP_GW_DATA is a mount point - enabled on an SD-only card, it would put the database on the SD card"
		fi
	fi

	wiretap_gw_images=$(sed -n 's/^[[:space:]]*image:[[:space:]]*//p' "$R/$WIRETAP_GW_COMPOSE" 2>/dev/null)
	wiretap_gw_unpinned=$(printf '%s\n' "$wiretap_gw_images" |
		awk 'NF && (!/^[a-z0-9.\/_-]+:[A-Za-z0-9._-]*[0-9][A-Za-z0-9._-]*$/ || /:latest/)' | tr '\n' ' ')
	if [ -z "$wiretap_gw_images" ]; then
		fail WIRETAP-12 "/$WIRETAP_GW_COMPOSE is missing or names no image"
	elif [ -n "$wiretap_gw_unpinned" ]; then
		fail WIRETAP-12 "the gateway's compose file has an image not pinned to a version: $wiretap_gw_unpinned"
	else
		ok WIRETAP-12 "the gateway's compose file pins every image"
	fi

	wiretap_gw_boot=$(find "$R"/etc/systemd/system/*.wants -name docker.service -o -name containerd.service 2>/dev/null)
	if [ -n "$wiretap_gw_boot" ]; then
		fail WIRETAP-13 "dockerd starts at boot - it belongs to the gateway, which ships off: ${wiretap_gw_boot#"$R"}"
	else
		ok WIRETAP-13 "dockerd waits for its socket"
	fi

	if [ -x "$R/$WIRETAP_GW_DOCKER" ] && [ -x "$R/$WIRETAP_GW_PLUGIN" ]; then
		ok WIRETAP-14 "docker and its compose plugin are installed"
	else
		fail WIRETAP-14 "docker or its compose plugin is missing - enabling the gateway would fail"
	fi
}

fixture() {
	wiretap_gw_stage="$SCAFFOLD/image/stage-appliance/20-gateway/files"
	mkdir -p "$T/${WIRETAP_GW_COMPOSE%/*}" "$T$WIRETAP_GW_DATA" "$T/${WIRETAP_GW_PLUGIN%/*}" \
		"$T/etc/systemd/system/sockets.target.wants"
	cp "$wiretap_gw_stage/wiretap-gateway.service" "$T/usr/lib/systemd/system/"
	cp "$wiretap_gw_stage/compose.yaml" "$T/$WIRETAP_GW_COMPOSE"
	printf '#!/bin/sh\n' > "$T/$WIRETAP_GW_DOCKER"
	printf '#!/bin/sh\n' > "$T/$WIRETAP_GW_PLUGIN"
	chmod 0755 "$T/$WIRETAP_GW_DOCKER" "$T/$WIRETAP_GW_PLUGIN"
	: > "$T/usr/lib/systemd/system/docker.socket"
	ln -s /usr/lib/systemd/system/docker.socket "$T/etc/systemd/system/sockets.target.wants/"
}

# shellcheck disable=SC2016 # each mutation is expanded when it runs, in the tree
cases() {
	case_run WIRETAP-10 "the gateway unit is missing" \
		'rm -f "usr/lib/systemd/system/$WIRETAP_GW_UNIT"'
	case_run WIRETAP-10 "the gateway enabled in the image" \
		'ln -s "/usr/lib/systemd/system/$WIRETAP_GW_UNIT" etc/systemd/system/multi-user.target.wants/'
	case_run WIRETAP-11 "the gateway would start without its storage" \
		'resed "/^AssertPathIsMountPoint=/d" "usr/lib/systemd/system/$WIRETAP_GW_UNIT"'
	case_run WIRETAP-11 "the assertion names another path" \
		'resed "s|^AssertPathIsMountPoint=.*|AssertPathIsMountPoint=/srv|" "usr/lib/systemd/system/$WIRETAP_GW_UNIT"'
	case_run WIRETAP-12 "the compose file is missing" \
		'rm -f "$WIRETAP_GW_COMPOSE"'
	case_run WIRETAP-12 "an image on latest" \
		'resed "s|wiretap-backend:[^[:space:]]*|wiretap-backend:latest|" "$WIRETAP_GW_COMPOSE"'
	case_run WIRETAP-12 "a moving tag with a version in its name" \
		'resed "s|timescale/timescaledb:[^[:space:]]*|timescale/timescaledb:latest-pg16|" "$WIRETAP_GW_COMPOSE"'
	case_run WIRETAP-12 "an image with no tag" \
		'resed "s|timescale/timescaledb:[^[:space:]]*|timescale/timescaledb|" "$WIRETAP_GW_COMPOSE"'
	case_run WIRETAP-12 "an image named by a variable" \
		'resed "s|ghcr.io/wired-square/wiretap-backend:[^[:space:]]*|\${WIRETAP_IMAGE:-ghcr.io/wired-square/wiretap-backend:0.1.9}|" "$WIRETAP_GW_COMPOSE"'
	case_run WIRETAP-13 "docker.service enabled" \
		': > usr/lib/systemd/system/docker.service && ln -s /usr/lib/systemd/system/docker.service etc/systemd/system/multi-user.target.wants/'
	case_run WIRETAP-13 "containerd.service enabled" \
		': > usr/lib/systemd/system/containerd.service && ln -s /usr/lib/systemd/system/containerd.service etc/systemd/system/multi-user.target.wants/'
	case_run WIRETAP-14 "the compose plugin is missing" \
		'rm -f "$WIRETAP_GW_PLUGIN"'
}
