#!/bin/sh
#
# Print the registry mirror's read token, for the gateway image's build secret:
# CARGO_REGISTRIES_WIRED_SQUARE_MIRROR_TOKEN if it is set, otherwise the
# `wired-square-mirror` token in $CARGO_HOME/credentials.toml.
#
#   export CARGO_REGISTRIES_WIRED_SQUARE_MIRROR_TOKEN="$(scripts/cargo-mirror-token.sh)"
set -eu

if [ -n "${CARGO_REGISTRIES_WIRED_SQUARE_MIRROR_TOKEN:-}" ]; then
	printf '%s\n' "$CARGO_REGISTRIES_WIRED_SQUARE_MIRROR_TOKEN"
	exit
fi

python3 -I - "${CARGO_HOME:-$HOME/.cargo}/credentials.toml" <<'PY'
import sys, tomllib

try:
    with open(sys.argv[1], "rb") as f:
        print(tomllib.load(f)["registries"]["wired-square-mirror"]["token"])
except (OSError, KeyError, tomllib.TOMLDecodeError) as e:
    sys.exit(f"cargo-mirror-token: no wired-square-mirror token in {sys.argv[1]} ({e!r}); "
             "set CARGO_REGISTRIES_WIRED_SQUARE_MIRROR_TOKEN")
PY
