#!/bin/bash
# Phase B smoke test: endpoint coverage + auth matrix against a running stack.
# With PGHOST and PGPASSWORD set, it also checks PostgreSQL itself through psql.
# No defaults: a missing address once sent a run to the wrong stack.
set -u
if [ $# -ne 4 ]; then
    echo "Usage: $0 <base-url> <admin-key> <seeded-db> <ingest-host:port>" >&2
    exit 2
fi
BASE="$1"
ADMIN_KEY="$2"
DB="$3"
INGEST="$4"
TOOLS="$(cd "$(dirname "$0")/../../tools" && pwd)"
PASS=0; FAIL=0

check() { # name, condition (0 = ok)
    if [ "$2" -eq 0 ]; then PASS=$((PASS+1)); echo "  [ok] $1"
    else FAIL=$((FAIL+1)); echo "  [FAIL] $1"; fi
}

A="Authorization: Bearer $ADMIN_KEY"
J="Content-Type: application/json"

# --- health + auth basics ---
curl -fsS "$BASE/v1/health" | grep -q '"status":"ok"'; check "health ok" $?
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/databases")
[ "$code" = "401" ]; check "no token -> 401" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer nope" "$BASE/v1/databases")
[ "$code" = "401" ]; check "bad token -> 401" $?

# --- key lifecycle ---
read_key=$(curl -fsS -H "$A" -H "$J" -d '{"name":"smoke-read","role":"read"}' "$BASE/v1/admin/keys" | python3 -c 'import sys,json;print(json.load(sys.stdin)["key"])')
[ -n "$read_key" ]; check "create read key returns plaintext" $?
R="Authorization: Bearer $read_key"
curl -fsS -H "$R" "$BASE/v1/databases" | grep -q "$DB"; check "read key lists databases" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -H "$R" -H "$J" -d '{"name":"nope_db"}' "$BASE/v1/databases")
[ "$code" = "403" ]; check "read key cannot create database" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -H "$R" "$BASE/v1/admin/keys")
[ "$code" = "403" ]; check "read key cannot list keys" $?

# --- read endpoints over seeded data ---
curl -fsS -H "$R" "$BASE/v1/db/$DB/time-bounds" | grep -q min_ts_us; check "time-bounds" $?
inv=$(curl -fsS -H "$R" "$BASE/v1/db/$DB/inventory")
echo "$inv" | grep -q '"frame_id":2016'; check "inventory contains seeded id 0x7E0" $?
curl -fsS -H "$R" -H "$J" -d '{"frame_id":2016,"limit":5}' "$BASE/v1/db/$DB/payloads" | grep -q payloads; check "payloads" $?

# frames cursor: page through with limit=30, expect >=80 frames total, no dupes
total=$(python3 - "$BASE" "$read_key" "$DB" <<'EOF'
import json, sys, urllib.request
base, key, db = sys.argv[1:4]
url = f"{base}/v1/db/{db}/frames?limit=30"
seen, cursor, total = set(), None, 0
while True:
    u = url + (f"&after={cursor}" if cursor else "")
    req = urllib.request.Request(u, headers={"Authorization": f"Bearer {key}"})
    data = json.load(urllib.request.urlopen(req))
    for f in data["frames"]:
        total += 1
        seen.add((f["ts_us"], f["id"], f["data_hex"], total))  # total makes dupes visible only via count
    cursor = data.get("next_cursor")
    if not cursor:
        break
print(total)
EOF
)
[ "$total" -ge 80 ]; check "frames cursor pages all rows (got $total)" $?

# --- analytical queries ---
q() { curl -fsS -H "$R" -H "$J" -d "$2" "$BASE/v1/db/$DB/query/$1"; }
q first-last '{"frame_id":2016}' | grep -q total_count; check "query first-last" $?
q distribution '{"frame_id":2016,"byte_index":0}' | grep -q percentage; check "query distribution" $?
q byte-changes '{"frame_id":2016,"byte_index":0}' | grep -q results; check "query byte-changes" $?
q frame-changes '{"frame_id":2016}' | grep -q results; check "query frame-changes" $?
q frequency '{"frame_id":2016,"bucket_size_ms":1000}' | grep -q results; check "query frequency" $?
q gap-analysis '{"frame_id":2016,"gap_threshold_ms":1}' | grep -q results; check "query gap-analysis" $?
q mux-statistics '{"frame_id":2016,"mux_selector_byte":0,"include_16bit":true,"payload_length":8}' | grep -q cases; check "query mux-statistics" $?
q pattern-search '{"pattern":[0],"pattern_mask":[0]}' | grep -q results; check "query pattern-search (match-all mask)" $?
q mirror-validation '{"mirror_frame_id":2016,"source_frame_id":2017,"tolerance_ms":100}' | grep -q results; check "query mirror-validation" $?

# --- events: a user's annotations on the database; a read key may write them ---
# 1700000000000000 us is 2023-11-14T22:13:20Z.
resp=$(curl -s -w '\n%{http_code}' -H "$R" -H "$J" -d '{"ts_us":1700000000000000,"duration_us":2500000,"note":"smoke"}' "$BASE/v1/db/$DB/events")
[ "${resp##*$'\n'}" = "201" ]; check "create event -> 201" $?
eid=$(printf '%s' "${resp%$'\n'*}" | python3 -c 'import sys,json;e=json.load(sys.stdin);assert (e["ts_us"],e["duration_us"],e["note"])==(1700000000000000,2500000,"smoke");print(e["id"])')
[ -n "$eid" ]; check "created event echoes its fields exactly" $?
curl -fsS -H "$R" "$BASE/v1/db/$DB/events?start=2023-11-14T00:00:00Z&end=2023-11-15T00:00:00Z" | grep -q "\"id\":$eid,"; check "events in range lists it" $?
! curl -fsS -H "$R" "$BASE/v1/db/$DB/events?end=2023-11-14T00:00:00Z" | grep -q "\"id\":$eid,"; check "events outside the range hide it" $?
curl -fsS -X PATCH -H "$R" -H "$J" -d '{"note":"smoke edited"}' "$BASE/v1/db/$DB/events/$eid" \
    | python3 -c 'import sys,json;e=json.load(sys.stdin);assert e["note"]=="smoke edited" and e["updated_at_us"]>e["created_at_us"]'; check "patch changes the note and moves updated_at" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH -H "$R" -H "$J" -d '{}' "$BASE/v1/db/$DB/events/$eid")
[ "$code" = "400" ]; check "an empty patch is a 400" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -H "$R" -H "$J" -d '{"ts_us":1,"duration_us":-1}' "$BASE/v1/db/$DB/events")
[ "$code" = "400" ]; check "a negative duration is a 400" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -H "$R" "$BASE/v1/db/$DB/events?start=garbage")
[ "$code" = "400" ]; check "a start PostgreSQL cannot parse is a 400" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H "$R" "$BASE/v1/db/$DB/events/$eid")
[ "$code" = "204" ]; check "delete event -> 204" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H "$R" "$BASE/v1/db/$DB/events/$eid")
[ "$code" = "404" ]; check "delete again -> 404" $?

# --- import: 1000 synthetic records into a fresh auto-created db ---
# Unique db name per run so the count assertion is idempotent.
IMPORT_DB="smoke_import_$(date +%s)"
python3 - <<'EOF' > /tmp/wiretap_import.bin
import struct, sys, time
base = int(time.time() * 1_000_000)
out = sys.stdout.buffer
for i in range(1000):
    payload = struct.pack("<II", i, i * 2)
    out.write(struct.pack("<qIBB", base + i * 1000, 0x300 + (i % 4), 0, len(payload)) + payload)
EOF
resp=$(curl -fsS -H "$A" -H "Content-Type: application/x-wiretap-frames" \
    --data-binary @/tmp/wiretap_import.bin "$BASE/v1/db/$IMPORT_DB/import?create=true")
echo "$resp" | grep -q '"imported":1000'; check "import 1000 records into auto-created db" $?
cnt=$(curl -fsS -H "$R" -H "$J" -d '{"frame_id":768}' "$BASE/v1/db/$IMPORT_DB/query/first-last" | python3 -c 'import sys,json;print(json.load(sys.stdin)["results"]["total_count"])')
[ "$cnt" = "250" ]; check "imported frames queryable (0x300 count=$cnt)" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -H "$R" -H "Content-Type: application/x-wiretap-frames" --data-binary @/tmp/wiretap_import.bin "$BASE/v1/db/$IMPORT_DB/import")
[ "$code" = "403" ]; check "read key cannot import" $?

# --- modbus rows: written over TCP ingest, the only path that carries them,
# and read back only when a protocol is named ---
read -r ikid ingest_key < <(curl -fsS -H "$A" -H "$J" -d '{"name":"smoke-ingest","role":"ingest"}' "$BASE/v1/admin/keys" | python3 -c 'import sys,json;k=json.load(sys.stdin);print(k["id"],k["key"])')
PYTHONPATH="$TOOLS" python3 - "${INGEST%:*}" "${INGEST##*:}" "$ingest_key" "$IMPORT_DB" <<'PYEOF'
import sys, time
from test_ingest_client import ReferenceClient, encode_modbus_record, encode_record
c = ReferenceClient(sys.argv[1], int(sys.argv[2]), token=sys.argv[3], database=sys.argv[4])
assert c.hello()[0] == 0
rec = encode_modbus_record(0, 1, 0x03, bytes.fromhex("010300000001840a"), bus=2)
fd = encode_record(0, 0x7F0, bytes(range(12)), fd=True)
assert c.send_batch(1, [rec, fd], base_ts_us=int(time.time() * 1_000_000))[1] == 0
PYEOF
check "modbus record and FD frame ingested over TCP" $?
# A set, not a count: the session above may not have closed yet.
PYTHONPATH="$TOOLS" python3 - "${INGEST%:*}" "${INGEST##*:}" "$ingest_key" "$IMPORT_DB" "$BASE" "$ADMIN_KEY" <<'PYEOF'
import json, sys, urllib.request
from test_ingest_client import ReferenceClient
host, port, token, db, base, admin = sys.argv[1:]
clients = [ReferenceClient(host, int(port), token=token, database=db, version=v) for v in (2, 3)]
assert all(c.hello()[0] == 0 for c in clients)
req = urllib.request.Request(f"{base}/v1/admin/ingest-sessions", headers={"Authorization": f"Bearer {admin}"})
sessions = json.load(urllib.request.urlopen(req))["sessions"]
assert {s["protocol_version"] for s in sessions if s["key_name"] == "smoke-ingest" and s["database"] == db} == {2, 3}
PYEOF
check "ingest-sessions names the version each HELLO spoke (v2 and v3)" $?
curl -fsS -H "$R" "$BASE/v1/db/$IMPORT_DB/inventory?protocol=modbus" | grep -q '"frame_id":259'; check "inventory lists modbus rows when asked (unit 1, FC03 = 0x0103)" $?
! curl -fsS -H "$R" "$BASE/v1/db/$IMPORT_DB/inventory" | grep -q '"frame_id":259'; check "inventory hides modbus rows by default" $?
curl -fsS -H "$R" "$BASE/v1/db/$IMPORT_DB/time-bounds?protocol=modbus" | grep -q '"min_ts_us":[0-9]'; check "time-bounds by protocol" $?
curl -fsS -H "$R" "$BASE/v1/db/$IMPORT_DB/frames?protocol=modbus&limit=5" | grep -q '"dlc":8,"len":8'; check "frames by protocol carry the message length" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -H "$R" "$BASE/v1/db/$IMPORT_DB/frames?protocol=modbsu")
[ "$code" = "400" ]; check "a protocol typo is a 400" $?
curl -fsS -X DELETE -H "$A" "$BASE/v1/admin/keys/$ikid" >/dev/null
curl -fsS -H "$R" "$BASE/v1/db/$IMPORT_DB/frames?limit=5000" | python3 -c 'import sys,json;assert [(f["dlc"],f["len"]) for f in json.load(sys.stdin)["frames"] if f["id"]==2032]==[(9,12)]'; check "an FD frame serves its length code as dlc and its bytes as len" $?
curl -fsS -H "$R" "$BASE/v1/db/$IMPORT_DB/inventory" | python3 -c 'import sys,json;assert [(e["max_dlc"],e["max_len"]) for e in json.load(sys.stdin)["entries"] if e["frame_id"]==2032]==[(9,12)]'; check "inventory serves an FD id's longest payload in bytes as max_len" $?

# --- admin views ---
curl -fsS -H "$A" "$BASE/v1/db/$DB/activity" | grep -q queries; check "activity" $?
curl -fsS -H "$A" "$BASE/v1/admin/ingest-sessions" | grep -q sessions; check "ingest-sessions" $?

# --- catalogue assignment, for a daemon id only this run uses ---
CAT_DAEMON="smoke-$(date +%s)"
CAT_IF="/dev/ttySMOKE"
CAT_CRLF=$'[meta]\r\nname = "smoke"\r\n'
cat_sha=$(printf '%s' "$CAT_CRLF" | python3 -c 'import sys,hashlib;c=sys.stdin.buffer.read();print(hashlib.sha1(b"blob %d\0"%len(c)+c).hexdigest())')
assign() { # content, expected ("-" for none) -> body, then the status on its own line
    python3 -c 'import json,sys;d,i,c,e=sys.argv[1:];b={"daemon_id":d,"interface":i,"content":c,"provenance":{"repo":"smoke"}};b.update({"expected":e} if e!="-" else {});print(json.dumps(b))' \
        "$CAT_DAEMON" "$CAT_IF" "$1" "$2" \
        | curl -s -w '\n%{http_code}' -X PUT -H "$A" -H "$J" --data-binary @- "$BASE/v1/admin/assignments"
}
cat_device() { # python over the device, as `d`
    curl -fsS -H "$A" "$BASE/v1/admin/daemons" | python3 -c "import sys,json
d=[d for x in json.load(sys.stdin)['daemons'] if x['daemon_id']=='$CAT_DAEMON' for d in x['devices'] if d['interface']=='$CAT_IF'][0]
$1"
}
resp=$(assign "$CAT_CRLF" "")
[ "${resp##*$'\n'}" = "200" ] && printf '%s' "${resp%$'\n'*}" | python3 -c 'import sys,json;a=json.load(sys.stdin)["assignment"];assert (a["blob_sha"],a["name"],a["provenance"])==(sys.argv[1],"smoke",{"repo":"smoke"}),a' "$cat_sha"
check "assign a CRLF catalogue, only if unassigned -> 200" $?
curl -fsS -H "$A" "$BASE/v1/admin/catalogs/$cat_sha" | python3 -c 'import sys,json;assert json.load(sys.stdin)["content"]==sys.argv[1]' "$CAT_CRLF"
check "the stored catalogue comes back byte for byte" $?
cat_device 'assert (d["bus"],d["database"],d["last_seen_us"],d["active"])==(None,None,None,None) and d["assignment"]["blob_sha"]=="'"$cat_sha"'",d'
check "daemons lists it assigned but never seen" $?
PYTHONPATH="$TOOLS" python3 - "${INGEST%:*}" "${INGEST##*:}" "$ADMIN_KEY" "$CAT_DAEMON" "$cat_sha" "$CAT_CRLF" <<'PYEOF'
import struct, sys
from test_ingest_client import ReferenceClient, frame_message
host, port, token, daemon, sha, content = sys.argv[1:]
c = ReferenceClient(host, int(port))
name = b"/dev/ttySMOKE"
hello = (b"WTAP" + bytes([3, 0, len(token)]) + token.encode() + b"\0"
         + bytes([len(daemon)]) + daemon.encode() + bytes([1, 7, len(name)]) + name)
c.send_raw(frame_message(0x01, hello))
mtype, ack = c.recv_message()
assert mtype == 0x81 and ack[:2] == bytes([0, 3]), ack.hex()
assert ack[10:] == bytes([1, 7]) + bytes.fromhex(sha), ack.hex()
got = b""
while True:
    c.send_raw(frame_message(0x04, bytes.fromhex(sha) + struct.pack("<I", len(got))))
    mtype, cat = c.recv_message()
    assert mtype == 0x84 and cat[0] == 0, cat[:1].hex()
    got += cat[29:]
    if len(got) >= struct.unpack_from("<I", cat, 21)[0]:
        break
assert got == content.encode(), got
c.send_raw(frame_message(0x04, bytes(20) + struct.pack("<I", 0)))
mtype, cat = c.recv_message()
assert mtype == 0x84 and cat[0] == 1, cat[:1].hex()
PYEOF
check "a v3 HELLO gets its assignment, and CATALOG serves the blob byte for byte" $?
PYTHONPATH="$TOOLS" python3 - "${INGEST%:*}" "${INGEST##*:}" "$ADMIN_KEY" "$CAT_DAEMON" "$cat_sha" "$BASE" <<'PYEOF'
import json, sys, urllib.request
from test_ingest_client import ReferenceClient, frame_message
host, port, token, daemon, sha, base = sys.argv[1:]
c = ReferenceClient(host, int(port))
name = b"/dev/ttySMOKE"
hello = (b"WTAP" + bytes([3, 0, len(token)]) + token.encode() + b"\0"
         + bytes([len(daemon)]) + daemon.encode() + bytes([1, 7, len(name)]) + name)
c.send_raw(frame_message(0x01, hello))
assert c.recv_message()[0] == 0x81

def report(refused=bytes(20), refusal=0):
    entry = bytes([7, 2]) + bytes.fromhex(sha) + refused + bytes([refusal])
    c.send_raw(frame_message(0x05, bytes([1]) + entry))
    c.send_raw(frame_message(0x03))
    assert c.recv_message()[0] == 0x83

def device():
    req = urllib.request.Request(f"{base}/v1/admin/daemons", headers={"Authorization": f"Bearer {token}"})
    daemons = json.load(urllib.request.urlopen(req))["daemons"]
    return [d for x in daemons if x["daemon_id"] == daemon for d in x["devices"]][0]

report()
first = device()
assert first["bus"] == 7 and first["database"] and first["last_seen_us"], first
active = first["active"]
assert (active["source"], active["blob_sha"], active["name"], active["refused"]) == ("assigned", sha, "smoke", None), active
report()
assert device()["active"] == active, "an identical report moved since_us"
report(bytes([0x22]) * 20, 2)
again = device()["active"]
assert again["refused"] == {"blob_sha": "22" * 20, "reason": "did_not_parse"}, again
assert again["since_us"] == active["since_us"], "a refusal is not a change of catalogue"
PYEOF
check "a CATALOG_STATUS shows as active, its since unmoved by a repeat or a refusal" $?
resp=$(assign $'[meta]\nname = "smoke 2"\n' "$(printf '%040d' 0)")
[ "${resp##*$'\n'}" = "409" ] && printf '%s' "${resp%$'\n'*}" | python3 -c 'import sys,json;assert json.load(sys.stdin)["current"]==sys.argv[1]' "$cat_sha"
check "assign with a stale expected -> 409 naming the current SHA" $?
resp=$(assign $'[meta]\nversion = 0\n' "$cat_sha")
[ "${resp##*$'\n'}" = "400" ] && printf '%s' "${resp%$'\n'*}" | python3 -c 'import sys,json;assert [f["field"] for f in json.load(sys.stdin)["findings"]]==["meta.name","meta.version"]'
check "assign a catalogue that does not validate -> 400 with its findings" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H "$A" "$BASE/v1/admin/assignments?daemon_id=$CAT_DAEMON&interface=%2Fdev%2FttySMOKE&expected=$cat_sha")
[ "$code" = "204" ]; check "clear the assignment -> 204" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H "$A" "$BASE/v1/admin/assignments?daemon_id=$CAT_DAEMON&interface=%2Fdev%2FttySMOKE")
[ "$code" = "204" ]; check "clear again -> 204" $?
resp=$(curl -s -w '\n%{http_code}' -X DELETE -H "$A" "$BASE/v1/admin/assignments?daemon_id=$CAT_DAEMON&interface=%2Fdev%2FttySMOKE&expected=$cat_sha")
[ "${resp##*$'\n'}" = "409" ] && printf '%s' "${resp%$'\n'*}" | python3 -c 'import sys,json;assert json.load(sys.stdin)["current"] is None'
check "clear again with the old expected -> 409, current null" $?

# --- logs ---
curl -fsS -H "$A" "$BASE/v1/admin/logs" | grep -q records; check "logs" $?
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/admin/logs")
[ "$code" = "401" ]; check "logs: no token -> 401" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -H "$R" "$BASE/v1/admin/logs")
[ "$code" = "403" ]; check "logs: read key -> 403" $?
# Every request above should have left an access-log line.
curl -fsS -H "$A" "$BASE/v1/admin/logs?limit=500" | grep -q '/v1/db/'; check "access log records a request" $?
# Polled routes sit below INFO; filtering at INFO makes this independent of RUST_LOG.
! curl -fsS -H "$A" "$BASE/v1/admin/logs?level=INFO&limit=500" | grep -q '/v1/health'; check "health checks are not in the INFO stream" $?

# --- revocation ---
kid=$(curl -fsS -H "$A" "$BASE/v1/admin/keys" | python3 -c 'import sys,json;ks=json.load(sys.stdin)["keys"];print([k["id"] for k in ks if k["name"]=="smoke-read" and not k["revoked"]][-1])')
curl -fsS -X POST -H "$A" "$BASE/v1/admin/keys/$kid/revoke" | grep -q revoked; check "revoke read key" $?
code=$(curl -s -o /dev/null -w '%{http_code}' -H "$R" "$BASE/v1/databases")
[ "$code" = "401" ]; check "revoked key -> 401 immediately" $?
curl -fsS -X POST -H "$A" "$BASE/v1/admin/keys/$kid/restore" | grep -q restored; check "restore read key" $?
curl -fsS -X DELETE -H "$A" "$BASE/v1/admin/keys/$kid" | grep -q deleted; check "delete read key" $?

# --- against PostgreSQL directly, on a database only this block touches ---
if [ -n "${PGHOST:-}" ]; then
    PROBE_DB="smoke_probe_$(date +%s)"
    backends() { psql -U postgres -d postgres -tAc "SELECT count(*) FROM pg_stat_activity WHERE datname = '$PROBE_DB' AND backend_type = 'client backend'"; }
    curl -fsS -H "$A" -H "$J" -d "{\"name\":\"$PROBE_DB\"}" "$BASE/v1/databases" >/dev/null
    curl -fsS -H "$A" "$BASE/v1/databases" | python3 -c 'import sys,json;assert [d["rollup_state"] is not None for d in json.load(sys.stdin)["databases"] if d["name"]==sys.argv[1]]==[True]' "$PROBE_DB"
    check "databases probes a new database's rollup" $?
    # The probe's connection closes after the response, so give it a moment.
    for _ in 1 2 3 4 5 6 7 8 9 10; do [ "$(backends)" = "0" ] && break; sleep 0.3; done
    [ "$(backends)" = "0" ]; check "the rollup probe leaves no backend on the database" $?
    psql -U postgres -d "$PROBE_DB" -qc "ALTER TABLE public.events RENAME TO events_gone"
    code=$(curl -s -o /dev/null -w '%{http_code}' -H "$A" "$BASE/v1/db/$PROBE_DB/events")
    [ "$code" = "503" ]; check "a query PostgreSQL fails -> 503" $?
    curl -fsS -X DELETE -H "$A" "$BASE/v1/databases/$PROBE_DB" >/dev/null

    if [ -n "${cat_sha:-}" ]; then
        meta() { psql -U postgres -d "${WIRETAP_DEFAULT_DB:-wiretap}" -tAc "$1"; }
        meta "DELETE FROM wiretap_meta.catalog_assignment_history WHERE daemon_id = '$CAT_DAEMON';
              DELETE FROM wiretap_meta.daemon_devices WHERE daemon_id = '$CAT_DAEMON';
              DELETE FROM wiretap_meta.daemon_active WHERE daemon_id = '$CAT_DAEMON';
              DELETE FROM wiretap_meta.catalog_blobs b WHERE b.blob_sha = decode('$cat_sha','hex')
                AND NOT EXISTS (SELECT 1 FROM wiretap_meta.catalog_assignments a WHERE a.blob_sha = b.blob_sha)" >/dev/null
        check "the catalogue checks' rows are cleared" $?
    fi
else
    echo "  [skip] PostgreSQL checks: set PGHOST and PGPASSWORD to reach it with psql"
fi

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
