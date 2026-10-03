# What 10-wiretap installs, and the CAN and radio state a card ships in.

WIRETAP_STATUS=var/lib/dpkg/status
WIRETAP_EXTENDED=var/lib/apt/extended_states
WIRETAP_BIN=usr/bin/wiretap-server
WIRETAP_UNIT=wiretap-server.service
WIRETAP_CAN_D=etc/wiretap-server/can.d
WIRETAP_CONFIG_TXT=boot/firmware/config.txt
WIRETAP_CMDLINE=boot/firmware/cmdline.txt

# wiretap_paragraph <file> <package> <awk regex the paragraph must also match>
wiretap_paragraph() {
	awk -v pkg="$2" -v want="$3" 'BEGIN { RS = "" }
		$0 ~ ("(^|\n)Package: " pkg "(\n|$)") && $0 ~ want { found = 1 }
		END { exit !found }' "$1" 2>/dev/null
}
wiretap_installed() { wiretap_paragraph "$R/$WIRETAP_STATUS" "$1" '(^|\n)Status: install ok installed(\n|$)'; }

check() {
	if ! wiretap_installed wiretap-server; then
		fail WIRETAP-01 "dpkg does not have wiretap-server installed - 10-wiretap did not run, or the install failed"
	elif [ ! -x "$R/$WIRETAP_BIN" ]; then
		fail WIRETAP-01 "/$WIRETAP_BIN is missing or not executable"
	else
		ok WIRETAP-01 "wiretap-server is installed"
	fi

	if wiretap_paragraph "$R/$WIRETAP_EXTENDED" wiretap-server '(^|\n)Auto-Installed: 1'; then
		fail WIRETAP-02 "wiretap-server is marked automatically installed - export-image's autoremove would purge it (was it installed with dpkg -i?)"
	else
		ok WIRETAP-02 "wiretap-server is marked manually installed"
	fi

	if enabled "$WIRETAP_UNIT"; then
		ok WIRETAP-03 "$WIRETAP_UNIT is enabled"
	else
		fail WIRETAP-03 "$WIRETAP_UNIT is not enabled - the card would boot without capturing"
	fi

	wiretap_can=$(ls -A "$R/$WIRETAP_CAN_D" 2>/dev/null | tr '\n' ' ')
	if [ ! -d "$R/$WIRETAP_CAN_D" ]; then
		fail WIRETAP-04 "/$WIRETAP_CAN_D does not exist"
	elif [ -n "$wiretap_can" ]; then
		fail WIRETAP-04 "/$WIRETAP_CAN_D is not empty - an interface's settings belong to the card's owner: $wiretap_can"
	else
		ok WIRETAP-04 "/$WIRETAP_CAN_D exists and is empty"
	fi

	if grep -qiE '^[[:space:]]*dtoverlay=mcp251' "$R/$WIRETAP_CONFIG_TXT" 2>/dev/null; then
		fail WIRETAP-05 "config.txt enables a CAN controller - a HAT is chosen on the card, from the browser"
	else
		ok WIRETAP-05 "config.txt enables no CAN controller"
	fi

	wiretap_hits=$(cat "$R"/etc/modprobe.d/*.conf "$R"/lib/modprobe.d/*.conf "$R"/usr/lib/modprobe.d/*.conf 2>/dev/null |
		grep -E '^[[:space:]]*(blacklist|install)[[:space:]]+(brcmfmac|btbcm)([[:space:]]|$)' || true)
	if [ -z "$wiretap_hits" ] && [ -f "$R/$WIRETAP_CMDLINE" ] &&
		grep -qE '(modprobe\.blacklist|module_blacklist)=[^[:space:]]*(brcmfmac|btbcm)' "$R/$WIRETAP_CMDLINE"; then
		wiretap_hits="cmdline.txt"
	fi
	if [ -n "$wiretap_hits" ]; then
		fail WIRETAP-06 "a radio's driver is blacklisted - the Network screen could never turn it back on: $wiretap_hits"
	else
		ok WIRETAP-06 "no radio driver is blacklisted"
	fi

	if wiretap_installed can-utils; then
		ok WIRETAP-07 "can-utils is installed"
	else
		fail WIRETAP-07 "can-utils is not installed - candump and cansend are what a CAN fault is diagnosed with"
	fi
}

fixture() {
	mkdir -p "$T/${WIRETAP_STATUS%/*}" "$T/${WIRETAP_BIN%/*}" "$T/$WIRETAP_CAN_D"
	printf 'Package: wiretap-server\nStatus: install ok installed\nArchitecture: arm64\n\nPackage: can-utils\nStatus: install ok installed\nArchitecture: arm64\n\n' >> "$T/$WIRETAP_STATUS"
	printf '#!/bin/sh\n' > "$T/$WIRETAP_BIN"
	chmod 0755 "$T/$WIRETAP_BIN"
	: > "$T/usr/lib/systemd/system/$WIRETAP_UNIT"
	ln -s "/usr/lib/systemd/system/$WIRETAP_UNIT" "$T/etc/systemd/system/multi-user.target.wants/"
}

# shellcheck disable=SC2016 # each mutation is expanded when it runs, in the tree
cases() {
	case_run WIRETAP-01 "wiretap-server not installed" \
		'resed "/^Package: wiretap-server/,/^\$/d" "$WIRETAP_STATUS"'
	case_run WIRETAP-01 "wiretap-server removed, its conffiles left" \
		'printf "Package: wiretap-server\nStatus: deinstall ok config-files\n\nPackage: can-utils\nStatus: install ok installed\n" > "$WIRETAP_STATUS"'
	case_run WIRETAP-01 "the binary is missing" \
		'rm -f "$WIRETAP_BIN"'
	case_run WIRETAP-02 "wiretap-server marked automatically installed" \
		'mkdir -p "${WIRETAP_EXTENDED%/*}" && printf "Package: wiretap-server\nArchitecture: arm64\nAuto-Installed: 1\n" > "$WIRETAP_EXTENDED"'
	case_pass "WIRETAP-02 lets another package be automatic" \
		'mkdir -p "${WIRETAP_EXTENDED%/*}" && printf "Package: iproute2\nArchitecture: arm64\nAuto-Installed: 1\n\nPackage: wiretap-server\nArchitecture: arm64\nAuto-Installed: 0\n" > "$WIRETAP_EXTENDED"'
	case_run WIRETAP-03 "wiretap-server.service not enabled" \
		'rm -f "etc/systemd/system/multi-user.target.wants/$WIRETAP_UNIT"'
	case_run WIRETAP-04 "can.d missing" \
		'rmdir "$WIRETAP_CAN_D"'
	case_run WIRETAP-04 "an interface configured in the image" \
		'printf "BITRATE=500000\n" > "$WIRETAP_CAN_D/can0.conf"'
	case_run WIRETAP-05 "an MCP2515 overlay in config.txt" \
		'printf "dtparam=spi=on\ndtoverlay=mcp2515-can0,oscillator=16000000,interrupt=23\n" >> "$WIRETAP_CONFIG_TXT"'
	case_run WIRETAP-05 "an MCP251xFD overlay, indented" \
		'printf "  dtoverlay=mcp251xfd,spi0-0,interrupt=25\n" >> "$WIRETAP_CONFIG_TXT"'
	case_pass "WIRETAP-05 ignores a commented-out overlay" \
		'printf "#dtoverlay=mcp2515-can0\n" >> "$WIRETAP_CONFIG_TXT"'
	case_run WIRETAP-06 "brcmfmac blacklisted in /etc" \
		'mkdir -p etc/modprobe.d && printf "blacklist brcmfmac\n" > etc/modprobe.d/radios.conf'
	case_run WIRETAP-06 "btbcm installed as /bin/false under /usr/lib" \
		'mkdir -p usr/lib/modprobe.d && printf "install btbcm /bin/false\n" > usr/lib/modprobe.d/radios.conf'
	case_run WIRETAP-06 "brcmfmac blacklisted on the kernel command line" \
		'printf "console=tty1 root=PARTUUID=0-02 modprobe.blacklist=bcm2835_v4l2,brcmfmac\n" > "$WIRETAP_CMDLINE"'
	case_pass "WIRETAP-06 ignores a commented-out blacklist and a clean command line" \
		'mkdir -p etc/modprobe.d && printf "# blacklist brcmfmac\n" > etc/modprobe.d/radios.conf && printf "console=tty1 root=PARTUUID=0-02\n" > "$WIRETAP_CMDLINE"'
	case_run WIRETAP-07 "can-utils not installed" \
		'resed "/^Package: can-utils/,/^\$/d" "$WIRETAP_STATUS"'
}
