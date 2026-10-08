#!/bin/sh
# Chassis-owned. Run by wiretap-appliance-console.service at start and on each address
# or link change: writes the console login screen's address lines, and has
# agetty reprint the screen when they changed.
#
# The addresses avahi publishes the box's name on: interfaces up and multicast,
# not loopback or point-to-point, so never a tailnet's; per interface its global
# IPv4 addresses, or its 169.254 one while it has none, and its global IPv6 ones
# that are neither tentative nor deprecated. fe80:: is left out: a browser
# cannot use one without a zone.
OUT='/run/wiretap-appliance-addresses.issue'

{ ip -o link show; echo; ip -o addr show; } 2>/dev/null | awk '
	function listed(name) { if (!(name in seen)) { seen[name] = 1; order[++n] = name } }
	!NF { addresses = 1; next }
	!addresses {
		name = $2; sub(/:$/, "", name); sub(/@.*/, "", name)
		if ($3 ~ /[<,]UP[,>]/ && $3 ~ /[<,]MULTICAST[,>]/ && $3 !~ /LOOPBACK|POINTOPOINT/) up[name] = 1
		next
	}
	!($2 in up) || / (tentative|deprecated|dadfailed)/ { next }
	{ address = $4; sub(/\/.*/, "", address) }
	$3 == "inet" && address ~ /^169\.254\./ { fallback[$2] = fallback[$2] " " address; listed($2); next }
	$3 == "inet" { global[$2] = global[$2] " " address; listed($2); next }
	$3 == "inet6" && / scope global/ { six[$2] = six[$2] " " address; listed($2) }
	END {
		for (i = 1; i <= n; i++) {
			name = order[i]
			# agetty reads `\` as the start of an escape.
			count = split(name, part, "\\"); shown = part[1]
			for (j = 2; j <= count; j++) shown = shown "\\\\" part[j]
			four = (name in global) ? global[name] : fallback[name]
			count = split(four six[name], each, " ")
			for (j = 1; j <= count; j++) lines = lines sprintf("  %-8s %s\n", shown, each[j])
		}
		printf "%s\n", (lines == "" ? "No address on any network yet.\n" : "Addresses:\n" lines)
	}
' > "$OUT.new" || exit 0

if cmp -s "$OUT.new" "$OUT"; then
	rm -f "$OUT.new"
else
	mv "$OUT.new" "$OUT"
	agetty --reload 2>/dev/null
fi
exit 0
