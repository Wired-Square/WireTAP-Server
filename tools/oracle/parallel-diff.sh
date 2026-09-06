#!/usr/bin/env bash
#
# Diff the Rust capture against the Python oracle's, over one identical window.
#
# Usage:
#   tools/oracle/parallel-diff.sh <rust-db> <py-db> [window-start]
#
# Both database names are required: defaulting them would silently compare the
# wrong pair on a box carrying several captures.
#
# The parallel run is both readers on the same buses into two databases.
# SocketCAN is multicast, so two raw sockets each see every frame and neither
# reader costs the other anything. The oracle side is the container built from
# the Dockerfile beside this file:
#
#   docker build -t wiretap-oracle:parallel tools/oracle
#   docker run -d --name wiretap-oracle-parallel --network host \
#     --restart unless-stopped -v wiretap-backend_pgsocket:/var/run/postgresql \
#     wiretap-oracle:parallel -i can0,can1 -p 2323 --pg-enable \
#     --pg-dsn "host=/var/run/postgresql dbname=<py-db> user=postgres password=..."
#
# `--network host` because SocketCAN interfaces live in a network namespace and
# a bridged container sees no can0; the pgsocket volume because the gateway's
# compose file deliberately publishes no Postgres port; `-p 2323` because the
# Rust daemon owns 23. Create <py-db> with `POST /v1/databases` on the gateway
# so both sides get their schema from the same code path.
#
# Two ways to get the diff wrong, both of which reported a false difference
# before this script existed:
#
#   - Letting each side evaluate now(). Two psql calls run at different
#     instants, so a relative window is a DIFFERENT window on each side. The
#     bounds are computed once and passed as literals.
#   - Choosing a window that predates one capture's start. That reports the
#     younger side capturing a fraction as much. The window is bounded below,
#     and a pinned one that misses is refused.
#
# One difference is NOT a defect, and it is reported rather than hidden because
# a field exempted from a diff is a field nobody looks at:
#
#   - Remote-transmission frames. docs/porting-notes.md (Deliberate changes)
#     records that the Python archives RTR frames and the Rust skips them. A bus
#     carrying RTR differs in counts, payloads and dlc for that reason alone,
#     and the rows are NOT separable afterwards: the oracle masks the RTR flag
#     off the id and the kernel zero-fills the payload, so an archived RTR frame
#     is indistinguishable from a data frame carrying zeros. On such a bus this
#     gate cannot be won. Establish that before committing 48 hours to it:
#
#         candump can0 | head -2000 | grep -c ' R '     # 0 = no RTR
#
# Timestamps are NOT in that category. porting-notes files the two socket
# options under *Confirmed identical* — the same kernel receive time, truncated
# to microseconds either way — so a timestamp difference contradicts the port's
# own record and is worth chasing. Row and distinct-value counts are printed
# beside it for that reason.

set -euo pipefail

case "${1:-}" in
-h | --help | "")
	awk '/^[^#]/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"
	exit 0
	;;
esac

RUST_DB="$1"
PY_DB="${2:?usage: $0 <rust-db> <py-db> [window-start]}"
PIN="${3:-}"

# The pin reaches SQL as text, so it is checked here rather than trusted. psql
# only substitutes :'vars' when it lexes stdin or a file — not with -c — so the
# variable form silently sends `:'pin'` to the server, and a crafted argument
# would otherwise run as postgres. A timestamp has one shape; anything else is
# a mistake or an attack, and both deserve the same answer.
if [ -n "${PIN}" ] &&
	! [[ "${PIN}" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}[\ T][0-9]{2}:[0-9]{2}(:[0-9]{2}(\.[0-9]+)?)?([+-][0-9]{2}(:?[0-9]{2})?|Z)?$ ]]; then
	echo "window-start must be a timestamp like '2026-09-06 02:52:00+00', got: ${PIN}" >&2
	exit 2
fi

# Reaching the capture database from a script is already solved in this repo,
# and this follows crates/wiretap-backend/parity_test.py: PGHOST for a direct
# connection, else the compose service by name. `docker exec` on a generated
# container name is avoided deliberately — it depends on Compose spelling
# `<project>-<service>-1`, and it cannot reach a Postgres that is not in a
# container at all, which is how this directory's own README deploys one.
# -X so a ~/.psqlrc cannot print into a scalar result; ON_ERROR_STOP so a failed
# query is a failed command rather than an empty string that compares equal to
# another empty string; and a pinned datestyle because the bounds cross between
# two sessions — under `SQL, DMY` one side reads 06/09 as September and the
# other as June, and two disjoint windows are both empty, which used to pass.
PSQL_OPTS=(-X -q -tA -v ON_ERROR_STOP=1)
if [ -n "${PGHOST:-}" ]; then
	q() { local db="$1"; shift; PGOPTIONS='-cdatestyle=ISO,YMD' psql "${PSQL_OPTS[@]}" -U "${PGUSER:-postgres}" -d "$db" "$@"; }
else
	COMPOSE_DIR="${COMPOSE_DIR:-$(dirname "$0")/../../crates/wiretap-backend}"
	q() {
		local db="$1"
		shift
		(cd "${COMPOSE_DIR}" && docker compose exec -T -e PGOPTIONS='-cdatestyle=ISO,YMD' \
			timescaledb psql "${PSQL_OPTS[@]}" -U postgres -d "$db" "$@")
	}
fi

# Exit 2 is "cannot compare", exit 1 is "they disagree". A wrapper that cannot
# tell a typo'd database from a real divergence will eventually treat one as the
# other.
die() {
	echo "$*" >&2
	exit 2
}

# One query rather than four round trips: the bounds and every refusal condition
# are a single answer. The pin arrives as a psql variable, not as interpolated
# text — a crafted third argument otherwise runs as postgres, which was provable.
#
# Bounds come back as epoch integers so they cross between the two sessions as
# numbers rather than as locale-formatted text.
bounds="$(q "${PY_DB}" -c "
  WITH b AS (SELECT min(ts) AS first, max(ts) AS last FROM public.can_frame),
       w AS (SELECT CASE WHEN nullif('${PIN}','') IS NOT NULL
                         THEN date_trunc('minute', timestamptz '${PIN:-epoch}')
                         ELSE greatest(date_trunc('minute', now() - interval '3 minutes'),
                                       date_trunc('minute', (SELECT first FROM b)) + interval '1 minute')
                    END AS t0)
  SELECT extract(epoch from (SELECT t0 FROM w))::bigint,
         extract(epoch from (SELECT t0 FROM w) + interval '1 minute')::bigint,
         CASE WHEN (SELECT first FROM b) IS NULL
                   THEN 'no rows yet - is the oracle container running?'
              WHEN (SELECT t0 FROM w) < (SELECT first FROM b)
                   THEN 'window starts before this capture does'
              WHEN (SELECT t0 FROM w) + interval '1 minute' >= now() - interval '30 seconds'
                   THEN 'window has not closed yet'
              ELSE 'ok' END;")" || die "cannot query ${PY_DB}"

IFS='|' read -r T0 T1 why <<<"${bounds}"
[ -n "${why:-}" ] || die "could not determine a window from ${PY_DB}"
[ "${why}" = "ok" ] || die "${why}"
[[ "${T0}" =~ ^-?[0-9]+$ && "${T1}" =~ ^-?[0-9]+$ ]] || die "bad window bounds: ${T0} ${T1}"

W="ts >= to_timestamp(${T0}) AND ts < to_timestamp(${T1})"
echo "window: $(q "${PY_DB}" -c "select to_timestamp(${T0})||' -> '||to_timestamp(${T1});")"

# An empty window compares equal to another empty window: string_agg over no
# rows is NULL, md5(NULL) is NULL, and psql prints nothing for both sides. Six
# SAMEs and a clean exit on a capture that stopped is the worst outcome this
# script has, so emptiness is established before anything is compared.
#
# Requiring rows at or after T1 as well is what catches a disk-cache drain:
# cached frames keep their original ts and land long after the window closed, so
# a writer that has not yet passed T1 may still be filling it.
for db in "${RUST_DB}" "${PY_DB}"; do
	read -r n past <<<"$(q "$db" -c "select count(*)||' '||count(*) filter (where ts >= to_timestamp(${T1})) from public.can_frame where ts >= to_timestamp(${T0});")" ||
		die "cannot query ${db}"
	[ "${n:-0}" -gt 0 ] || die "${db} has no rows in the window - capture stopped, or draining a disk cache"
	[ "${past:-0}" -gt 0 ] || die "${db} has not written past the window yet - it may still be filling it"
done

fail=0

# Counts first: a difference here is frames rather than fields, and RTR is the
# first thing to rule out.
for col in bus id; do
	d="select string_agg(${col}||':'||c,'  ' order by ${col}) from (select ${col},count(*) c from public.can_frame where ${W} group by ${col}) t"
	r="$(q "${RUST_DB}" -c "$d")" || die "cannot query ${RUST_DB}"
	p="$(q "${PY_DB}" -c "$d")" || die "cannot query ${PY_DB}"
	if [ "$r" = "$p" ]; then
		echo "  SAME  count per ${col}"
	else
		echo "  DIFF  count per ${col}"
		[ "${col}" = "bus" ] && { echo "        rust:   ${r}"; echo "        python: ${p}"; }
		fail=1
	fi
done

# Then each field on its own, so a difference names itself instead of arriving
# as one opaque mismatch over every column at once.
#
# `dlc` is not a tautology: the Rust never stores it and the gateway recomputes
# it from the payload length, while the oracle stores what it read off the wire.
# `collate "C"` because the ordering inside string_agg decides the digest, and
# two databases created with different collations digest identical data
# differently.
#
# The results are assigned before they are compared: comparing two command
# substitutions inside `[ ]` suspends errexit, so two *failed* queries both
# yield "" and report SAME. That is the same false pass as an empty window,
# reached a different way.
check() {
	local d r p
	d="select md5(string_agg($2,'|' order by ($2) collate \"C\")) from public.can_frame where ${W}"
	r="$(q "${RUST_DB}" -c "$d")" || die "cannot query ${RUST_DB}"
	p="$(q "${PY_DB}" -c "$d")" || die "cannot query ${PY_DB}"
	if [ -z "$r" ] || [ -z "$p" ]; then
		die "no digest for '$1' - the query returned nothing"
	elif [ "$r" = "$p" ]; then
		echo "  SAME  $1"
	else
		echo "  DIFF  $1"
		fail=1
	fi
}
check "payload (id, bus, data)" "id||':'||bus||':'||encode(data_bytes,'hex')"
check "dlc" "id||':'||dlc"
check "flags (extended, is_fd, dir)" "id||':'||extended||is_fd||dir"
check "timestamps" "ts::text"

# "They differ" and "they differ on one row in 15037, and that row is a
# duplicate" are different findings, and only the second says where to look.
shape="select count(*)||' rows, '||count(distinct ts)||' distinct ts, lag '||coalesce((max(ingest_ts)-max(ts))::text,'-') from public.can_frame where ${W}"
echo "        rust:   $(q "${RUST_DB}" -c "${shape}")"
echo "        python: $(q "${PY_DB}" -c "${shape}")"

if [ "${fail}" -ne 0 ]; then
	cat >&2 <<'EOF'

Something disagrees. Before calling it a defect: does this bus carry RTR frames?
The Rust drops them and the oracle archives them, which moves counts, payloads
and dlc together. A timestamp-only difference has no such excuse.
EOF
	exit 1
fi
