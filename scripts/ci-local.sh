#!/usr/bin/env bash
#
# Run .github/workflows/ci.yml's jobs here, on a macOS host with Docker
# Desktop, under the names ci.yml gives them. GitHub runs ci.yml only on a
# release tag, so this is the CI for everything else.
#
#   scripts/ci-local.sh                     every job
#   scripts/ci-local.sh gates server-ui     the named jobs
#   scripts/ci-local.sh --list              the jobs and their names
#
# Capture path and gateway (vcan) is skipped: it needs the vcan kernel module,
# which Docker Desktop's kernel does not have, so it runs only on the tag. The
# package lifecycle runs in a throwaway Debian container booting systemd, for
# each architecture Docker can run here; the others wait for the tag too.
#
# Each job's output is also kept in target/ci-local/<job>.log.

# shellcheck disable=SC2329  # the job_ functions are called by name
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRATCH="$ROOT/target/ci-local"
JOB_TIMEOUT=30m
JOBS="gates capture package-amd64 package-arm64 image appliance appliance-browser
	appliance-packaging server-ui server-ui-browser server-image"

export CARGO_TERM_COLOR=always
export CARGO_NET_GIT_FETCH_WITH_CLI=true

job_name() {
	case $1 in
	gates) echo "Gates" ;;
	capture) echo "Capture path and gateway (vcan)" ;;
	package-amd64) echo "Package lifecycle (amd64)" ;;
	package-arm64) echo "Package lifecycle (arm64)" ;;
	image) echo "Gateway image" ;;
	appliance) echo "Appliance gates" ;;
	appliance-browser) echo "Appliance browser half" ;;
	appliance-packaging) echo "Appliance packaging" ;;
	server-ui) echo "Server UI gates" ;;
	server-ui-browser) echo "Server UI browser half" ;;
	server-image) echo "Server app image" ;;
	*) return 1 ;;
	esac
}

skip_reason() {
	case $1 in
	capture) echo "needs the vcan module, absent from Docker Desktop's kernel; runs on the tag" ;;
	package-*)
		docker run --rm --platform "linux/${1#package-}" debian:bookworm true >/dev/null 2>&1 ||
			echo "Docker here cannot run linux/${1#package-} containers; runs on the tag"
		;;
	esac
}

workspace_gates() {
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo clippy --workspace --all-targets --locked --target aarch64-unknown-linux-musl -- -D warnings
	cargo test --workspace --locked
}

browser_half() {
	cd "$1"
	npm ci
	npm run ci
}

package_lifecycle() {
	local arch=$1 deb
	# Changing it reruns wiretap-build-id, which no edit outside a crate's src/ does.
	WIRETAP_BUILD_ID="$(packaging/build-id.sh)" packaging/make-deb.sh --arch "$arch"
	# shellcheck disable=SC2012
	deb="$(ls -t target/deb/wiretap-server_*_"$arch".deb | head -1)"
	docker build --quiet --platform "linux/$arch" --tag "ci-local-systemd:$arch" - <<-'EOF'
		FROM debian:bookworm
		RUN apt-get update && apt-get install -y --no-install-recommends \
			systemd systemd-sysv udev adduser sqlite3 python3
		CMD ["/sbin/init"]
	EOF
	CONTAINER="ci-local-package-$arch-$$"
	docker run --detach --name "$CONTAINER" --platform "linux/$arch" \
		--privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
		--tmpfs /run --tmpfs /run/lock -v "$ROOT:/src:ro" "ci-local-systemd:$arch"
	docker exec "$CONTAINER" systemctl is-system-running --wait || true
	docker exec "$CONTAINER" /src/packaging/tests/deb-lifecycle.sh --yes "/src/$deb"
}

job_gates() {
	./scripts/check-release-contract.sh
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo clippy -p wiretap-server --target aarch64-unknown-linux-musl \
		--all-targets --locked -- -D warnings
	cargo test --workspace --locked
}

job_package_amd64() { package_lifecycle amd64; }
job_package_arm64() { package_lifecycle arm64; }

job_image() {
	docker buildx build \
		--file crates/wiretap-backend/Dockerfile \
		--build-arg "WIRETAP_BUILD_ID=$(packaging/build-id.sh)" \
		--output type=cacheonly \
		.
}

job_appliance() { cd appliance; workspace_gates; }
job_appliance_browser() { browser_half appliance/frontend; }

# Into target/, so the pinned appliance-xtask never replaces the one on PATH.
job_appliance_packaging() {
	cd appliance
	local rev
	rev="$(grep -om1 'wslib-appliance-rs\.git?[^#"]*#[0-9a-f]*' Cargo.lock | cut -d'#' -f2)"
	cargo install --locked --root "$SCRATCH/xtask" \
		--git https://github.com/Wired-Square/wslib-appliance-rs.git --rev "$rev" appliance-xtask
	PATH="$SCRATCH/xtask/bin:$PATH"
	appliance-xtask packaging diff
	scaffold/image/tests/run-verify-tests.sh
}

job_server_ui() { cd apps/wiretap-server; workspace_gates; }
job_server_ui_browser() { browser_half apps/wiretap-server/frontend; }

job_server_image() {
	(cd apps/wiretap-server/frontend && npm ci)
	apps/wiretap-server/make-image.sh amd64
}

if [ "${1:-}" = --run-job ]; then
	CONTAINER=""
	trap '[ -z "$CONTAINER" ] || docker rm -f "$CONTAINER" >/dev/null' EXIT
	trap 'exit 143' TERM INT
	cd "$ROOT"
	set -x
	"job_${2//-/_}"
	exit
fi

case "${1:-}" in
--list)
	for job in $JOBS; do printf '%-20s %s\n' "$job" "$(job_name "$job")"; done
	exit 0
	;;
-h | --help)
	awk 'NR > 2 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0"
	exit 0
	;;
esac

selected="${*:-$JOBS}"
for job in $selected; do
	job_name "$job" >/dev/null || { echo "unknown job: $job (see --list)" >&2; exit 2; }
done

mkdir -p "$SCRATCH"
summary=""
failed=0
for job in $selected; do
	reason="$(skip_reason "$job")"
	if [ -n "$reason" ]; then
		summary+="$(printf '%-34s %-8s %s' "$(job_name "$job")" skipped "$reason")"$'\n'
		continue
	fi
	printf '\n=== %s\n' "$(job_name "$job")"
	start=$SECONDS
	status=0
	timeout --kill-after=60 "$JOB_TIMEOUT" "$0" --run-job "$job" 2>&1 | tee "$SCRATCH/$job.log" ||
		status=${PIPESTATUS[0]}
	case $status in
	0) result=pass ;;
	124 | 137) result=timeout failed=1 ;;
	*) result=fail failed=1 ;;
	esac
	elapsed=$((SECONDS - start))
	summary+="$(printf '%-34s %-8s %dm%02ds' "$(job_name "$job")" "$result" $((elapsed / 60)) $((elapsed % 60)))"$'\n'
done

printf '\n%-34s %-8s %s\n%s' JOB RESULT TIME "$summary"
exit "$failed"
