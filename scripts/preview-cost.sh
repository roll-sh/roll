#!/usr/bin/env bash
# roll previews end to end, and what a preview costs (docs/previews.md, "Measured on this machine").
#
# Starts a rolld of its own (home under ~/.cache/roll-previews-e2e, ports 27xxx), makes a project
# with a repo and one of each data resource, turns previews on, opens a change, and then:
#   1. waits for the preview: a branch of every resource, the service deployed;
#   2. visits the service (cold start, then warm) and reads what the running container uses;
#   3. lets it sleep and reads what's left;
#   4. lands the change and checks that every branch went and every parent stayed.
# Prints the measurements as markdown. Needs Docker (the service container is roll-previews-*).
#
#   scripts/preview-cost.sh [--keep]     # --keep: leave the daemon running at the end
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${ROLLD:-$ROOT/target/debug/rolld}
HOME_DIR=${PREVIEW_E2E_HOME:-$HOME/.cache/roll-previews-e2e}
KEEP=${1:-}
API=127.0.0.1:27070
DOMAIN=preview.localhost
IMAGE=${PREVIEW_IMAGE:-python:3.13-alpine}

say() { printf '\033[2m%s\033[0m\n' "$*" >&2; }
api() { # api METHOD PATH [JSON]
  local m=$1 p=$2 b=${3:-}
  if [ -n "$b" ]; then curl -sS -X "$m" "http://$API$p" -H 'content-type: application/json' -d "$b"; else curl -sS -X "$m" "http://$API$p"; fi
}
jq_() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {'d': d}))" "$1"; }
now_ms() { date +%s%3N; }

cleanup() {
  if [ "$KEEP" != "--keep" ] && [ -n "${PID:-}" ]; then
    kill "$PID" 2>/dev/null || true
    # rolld drains on SIGTERM; don't wait on it for long.
    for _ in $(seq 1 20); do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done
    kill -9 "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
    for c in $(docker ps -aq --filter "label=sh.roll.code.home=$HOME_DIR"); do docker rm -f "$c" >/dev/null; done
  fi
}
trap cleanup EXIT


# Other daemons share this host: never talk to one of theirs.
if curl -s -o /dev/null --max-time 1 "http://$API/v1/info"; then echo "something already answers on $API: stop it first" >&2; exit 1; fi
rm -rf "$HOME_DIR" && mkdir -p "$HOME_DIR"
export ROLL_HOME=$HOME_DIR ROLL_API=$API ROLL_PROXY=127.0.0.1:27432 ROLL_REDIS=127.0.0.1:27380 ROLL_REDIS_HTTP=127.0.0.1:27081 \
  ROLL_KAFKA=127.0.0.1:27092 ROLL_S3=127.0.0.1:27900 ROLL_STORAGE_API=127.0.0.1:27901 \
  ROLL_QUEUES_AMQP=127.0.0.1:27672 ROLL_QUEUES_MANAGEMENT=127.0.0.1:27673 ROLL_QUEUES_STOMP=127.0.0.1:27613 ROLL_QUEUES_WEB_STOMP=127.0.0.1:27674 \
  ROLL_QUEUES_MQTT=127.0.0.1:27883 ROLL_QUEUES_WEB_MQTT=127.0.0.1:27675 ROLL_QUEUES_STREAM=127.0.0.1:27552 ROLL_QUEUES_HTTP=127.0.0.1:27680 \
  ROLL_NOSQL_CQL=127.0.0.1:27042 ROLL_NOSQL_CQL_SHARD_AWARE=127.0.0.1:27043 ROLL_NOSQL_ALTERNATOR=127.0.0.1:27000 ROLL_NOSQL_REST=127.0.0.1:27100 \
  ROLL_CODE_PREVIEW_DOMAIN=$DOMAIN ROLL_PRODUCTS=${PREVIEW_PRODUCTS:-code,redis,storage,nosql,queues,streams,tasks} RUST_LOG=${RUST_LOG:-info}
unset ROLL_ADMIN_TOKEN ROLL_CODE_TOKEN
say "starting rolld ($ROLL_PRODUCTS) in $HOME_DIR"
nice -n 10 "$BIN" >"$HOME_DIR/rolld.log" 2>&1 &
PID=$!
for _ in $(seq 1 120); do
  kill -0 "$PID" 2>/dev/null || { tail -20 "$HOME_DIR/rolld.log"; exit 1; }
  curl -sf "http://$API/v1/info" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -sf "http://$API/v1/info" >/dev/null || { tail -40 "$HOME_DIR/rolld.log"; exit 1; }
RSS0=$(ps -o rss= -p "$PID")

say "a repo with a service, and one of each data resource"
api POST /v1/code/owners '{"name":"acme","kind":"org"}' >/dev/null
api POST /v1/code/repos '{"owner":"acme","name":"web","source":"native"}' >/dev/null
TOML='[preview]\nimage = \"'"$IMAGE"'\"\nrun = \"python -m http.server $PORT\"\nport = 8000\nmemory = 128\ncpus = 0.25\n'
api POST /v1/code/repos/acme/web/land "{\"message\":\"init\",\"files\":{\"roll.toml\":{\"content\":\"$TOML\"},\"index.html\":{\"content\":\"main\"}}}" >/dev/null
REDIS=$(api POST /v1/redis/databases '{"name":"cache"}' | jq_ "d['id']")
api POST /v1/storage/buckets '{"name":"assets"}' >/dev/null
NOSQL=$(api POST /v1/nosql/databases '{"name":"events"}' | jq_ "d['id']")
api POST /v1/queues/vhosts '{"name":"jobs"}' >/dev/null
STREAM=$(api POST /v1/streams '{"name":"clicks"}' | jq_ "d['id']")
api POST "/v1/streams/$STREAM/topics" '{"name":"clicks","partitions":3}' >/dev/null
api POST /_roll/tenants '{"id":"work"}' >/dev/null
# Data in the parents, so the branches have something to share: 20 MiB of objects, 20,000 keys.
KEY=$(api POST /v1/storage/keys '{"name":"e2e","permission":"admin"}')
export AWS_ACCESS_KEY_ID=$(echo "$KEY" | jq_ "d['id']") AWS_SECRET_ACCESS_KEY=$(echo "$KEY" | jq_ "d['secret_access_key']") AWS_DEFAULT_REGION=us-east-1
S3="aws --endpoint-url http://127.0.0.1:27900 s3"
mkdir -p "$HOME_DIR/blobs" && for i in $(seq 1 20); do head -c 1048576 /dev/urandom >"$HOME_DIR/blobs/$i.bin"; done
$S3 cp --quiet --recursive "$HOME_DIR/blobs" s3://assets/blobs/
REDIS_URL=$(api GET "/v1/redis/databases/$REDIS" | jq_ "d['redis_url']")
resp() { # resp <redis url> <n keys> <prefix>: SET n keys of 1 KiB over RESP
  python3 - "$@" <<'PY'
import socket, sys, urllib.parse
u = urllib.parse.urlparse(sys.argv[1]); n = int(sys.argv[2]); prefix = sys.argv[3]
s = socket.create_connection((u.hostname, u.port))
def cmd(*a):
    return b"*%d\r\n" % len(a) + b"".join(b"$%d\r\n%s\r\n" % (len(x), x) for x in a)
buf = cmd(b"AUTH", u.username.encode(), urllib.parse.unquote(u.password).encode())
buf += b"".join(cmd(b"SET", f"{prefix}:{i}".encode(), b"x" * 1024) for i in range(n))
s.sendall(buf)
want, got = n + 1, b""
while got.count(b"\r\n") < want:
    got += s.recv(65536)
assert b"-" not in got[:1], got[:200]
PY
}
resp "$REDIS_URL" 20000 parent

PRJ=$(api POST /v1/workspace/projects '{"name":"shop"}' | jq_ "d['id']")
for r in "repo/acme%2Fweb" "redis/$REDIS" "bucket/assets" "nosql/$NOSQL" "queue/jobs" "stream/$STREAM" "tasks/work"; do
  api PUT "/v1/workspace/projects/$PRJ/resources/$r" '{"x":0,"y":0}' >/dev/null
done
api PUT "/v1/previews/projects/$PRJ/settings" '{"enabled":true,"sleep_after_s":60}' >/dev/null

say "open a change"
T0=$(now_ms)
CHG=$(api POST /v1/code/repos/acme/web/changes '{"title":"New page","message":"New page","base":"main","files":{"index.html":{"content":"preview of change 1"}}}' | jq_ "d['id']")
PV=""
for _ in $(seq 1 240); do
  L=$(api GET "/v1/previews?project=$PRJ")
  PV=$(echo "$L" | jq_ "d[0]['id'] if d and d[0]['state'] in ('ready','sleeping') else ''")
  [ -n "$PV" ] && break
  sleep 0.5
done
[ -n "$PV" ] || { echo "$L"; tail -30 "$HOME_DIR/rolld.log"; exit 1; }
T1=$(now_ms)
P=$(api GET "/v1/previews/$PV")
echo "$P" | python3 -m json.tool > "$HOME_DIR/preview.json"
HOST=$(echo "$P" | jq_ "d['services'][0]['url'].split('//')[1].split(':')[0]")
say "preview $PV ready in $((T1 - T0)) ms: $(echo "$P" | jq_ "[(r['kind'], r['ref']) for r in d['resources']]")"

say "write into the preview's branches: 1 MiB of objects, 1,000 keys"
BRANCH_BUCKET=$(echo "$P" | jq_ "[r['ref'] for r in d['resources'] if r['kind']=='bucket'][0]")
BRANCH_REDIS=$(echo "$P" | jq_ "[r['ref'] for r in d['resources'] if r['kind']=='redis'][0]")
$S3 cp --quiet "$HOME_DIR/blobs/1.bin" "s3://$BRANCH_BUCKET/new/1.bin"
resp "$(api GET "/v1/redis/databases/$BRANCH_REDIS" | jq_ "d['redis_url']")" 1000 preview
PARENT_OBJECTS=$(api GET /v1/storage/buckets/assets | jq_ "d['bytes']")
BRANCH_OBJECTS=$(api GET "/v1/storage/buckets/$BRANCH_BUCKET" | jq_ "d['bytes']")
BRANCH_SEES=$($S3 ls --recursive "s3://$BRANCH_BUCKET/" | wc -l)

say "visit it: cold start"
T2=$(now_ms)
BODY=$(curl -sS -H "Host: $HOST" "http://$API/" --max-time 120)
T3=$(now_ms)
echo "$BODY" | grep -q "preview of change 1" || { echo "unexpected body: $BODY"; tail -30 "$HOME_DIR/rolld.log"; exit 1; }
T4=$(now_ms); curl -sS -o /dev/null -H "Host: $HOST" "http://$API/"; T5=$(now_ms)
CONTAINER="roll-previews-$(echo "$HOST" | cut -d. -f1)"
STATS=$(docker stats --no-stream --format '{{.MemUsage}} {{.CPUPerc}}' "$CONTAINER")
RSS1=$(ps -o rss= -p "$PID")
api POST "/v1/previews/$PV/refresh" >/dev/null; sleep 3
ACTIVE=$(api GET "/v1/previews/$PV" | jq_ "d['cost']")

say "sleep it"
api POST "/v1/previews/$PV/sleep" >/dev/null
sleep 1
GONE=$(docker ps -q --filter "name=^$CONTAINER\$" | wc -l)
du_of() { du -sk "$HOME_DIR/$1" 2>/dev/null | cut -f1; }
OBJ_KB=$(du_of objects)
SLEEPING=$(api GET "/v1/previews/$PV" | jq_ "d['cost']")

say "land the change: the preview and its branches go, the parents stay"
api POST "/v1/code/changes/$CHG/land" '{}' >/dev/null
for _ in $(seq 1 120); do
  N=$(api GET "/v1/previews?project=$PRJ" | jq_ "len(d)")
  [ "$N" = 0 ] && break
  sleep 0.5
done
[ "$N" = 0 ] || { api GET "/v1/previews?project=$PRJ"; exit 1; }
PARENTS_OK=$( (api GET "/v1/redis/databases/$REDIS" | jq_ "d['id']"; api GET /v1/storage/buckets/assets | jq_ "d['name']"; api GET "/v1/nosql/databases/$NOSQL" | jq_ "d['id']") | wc -l)
BRANCHES_LEFT=$(api GET /v1/redis/databases | jq_ "len([x for x in d if x['name'].startswith('pv-')])")

cat <<EOF
| What | Measured |
| --- | --- |
| Change opened → preview ready ($(echo "$P" | jq_ "len(d['resources'])") branches made, the service deployed) | $((T1 - T0)) ms |
| Parent bucket bytes / the branch's own bytes after writing 1 MiB / objects the branch sees | $PARENT_OBJECTS / $BRANCH_OBJECTS / $BRANCH_SEES |
| First visit (cold start of the service container, image cached) | $((T3 - T2)) ms |
| Next visit (warm) | $((T5 - T4)) ms |
| Service container while running | $STATS |
| rolld RSS before → with the preview running | $((RSS0 / 1024)) MiB → $((RSS1 / 1024)) MiB |
| Containers left after sleep | $GONE |
| Object storage of the whole daemon after the preview (parents included) | ${OBJ_KB} KiB |
| Cost while running (from the preview) | $ACTIVE |
| Cost while asleep (from the preview) | $SLEEPING |
| After landing: parents present / preview Redis branches left | $PARENTS_OK of 3 / $BRANCHES_LEFT |
EOF
