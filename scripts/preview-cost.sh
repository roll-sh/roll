#!/usr/bin/env bash
# roll previews end to end, and what a preview costs (docs/previews.md, "Measured on this machine").
#
# Starts a rolld of its own (home under ~/.cache/roll-previews-e2e, ports 27xxx), makes a project
# with a repo and one of each data resource, turns previews on, opens a change, and then:
#   1. waits for the preview: a branch of every resource, the service deployed;
#   2. visits the service (cold start, then warm) and reads what the running container uses;
#   3. lets it sleep and reads what's left;
#   4. checks a preview's own variable, a docs-only push under ignore_paths (kept build), and
#      `[skip preview]` in the title;
#   5. lands the change and checks that every branch went and every parent stayed.
# Prints the measurements as markdown. Needs Docker (the service container is roll-previews-*).
#
#   scripts/preview-cost.sh [--keep]     # --keep: leave the daemon running at the end
#   STOP_BEFORE_LAND=1 scripts/preview-cost.sh --keep   # and the preview with it (to look at it)
set -euo pipefail
# The ports: <P>070 for the API and so on (27 unless PREVIEW_E2E_PORTS says another two digits).
P=${PREVIEW_E2E_PORTS:-27}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${ROLLD:-$ROOT/target/debug/rolld}
HOME_DIR=${PREVIEW_E2E_HOME:-$HOME/.cache/roll-previews-e2e}
KEEP=${1:-}
API=127.0.0.1:${P}070
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
    # Only what this daemon made: its service containers and its Postgres computes.
    for c in $(docker ps -aq --filter "label=sh.roll.code.home=$HOME_DIR") $(docker ps -aq --filter "label=roll.home=$HOME_DIR"); do docker rm -f "$c" >/dev/null; done
  fi
}
trap cleanup EXIT


# Other daemons share this host: never talk to one of theirs.
if curl -s -o /dev/null --max-time 1 "http://$API/v1/info"; then echo "something already answers on $API: stop it first" >&2; exit 1; fi
rm -rf "$HOME_DIR" && mkdir -p "$HOME_DIR"
export ROLL_HOME=$HOME_DIR ROLL_API=$API ROLL_PROXY=127.0.0.1:${P}432 ROLL_REDIS=127.0.0.1:${P}380 ROLL_REDIS_HTTP=127.0.0.1:${P}081 \
  ROLL_KAFKA=127.0.0.1:${P}092 ROLL_S3=127.0.0.1:${P}900 ROLL_STORAGE_API=127.0.0.1:${P}901 \
  ROLL_QUEUES_AMQP=127.0.0.1:${P}672 ROLL_QUEUES_MANAGEMENT=127.0.0.1:${P}673 ROLL_QUEUES_STOMP=127.0.0.1:${P}613 ROLL_QUEUES_WEB_STOMP=127.0.0.1:${P}674 \
  ROLL_QUEUES_MQTT=127.0.0.1:${P}883 ROLL_QUEUES_WEB_MQTT=127.0.0.1:${P}675 ROLL_QUEUES_STREAM=127.0.0.1:${P}552 ROLL_QUEUES_HTTP=127.0.0.1:${P}680 \
  ROLL_NOSQL_CQL=127.0.0.1:${P}042 ROLL_NOSQL_CQL_SHARD_AWARE=127.0.0.1:${P}043 ROLL_NOSQL_ALTERNATOR=127.0.0.1:${P}000 ROLL_NOSQL_REST=127.0.0.1:${P}100 \
  ROLL_CODE_PREVIEW_DOMAIN=$DOMAIN ROLL_PRODUCTS=${PREVIEW_PRODUCTS:-code,postgres,redis,storage,nosql,queues,streams,tasks} RUST_LOG=${RUST_LOG:-info}
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
TOML='[preview]\nimage = \"'"$IMAGE"'\"\nrun = \"env > env.txt && python -m http.server $PORT\"\nport = 8000\nmemory = 128\ncpus = 0.25\n'
api POST /v1/code/repos/acme/web/land "{\"message\":\"init\",\"files\":{\"roll.toml\":{\"content\":\"$TOML\"},\"index.html\":{\"content\":\"main\"}}}" >/dev/null
REDIS=$(api POST /v1/redis/databases '{"name":"cache"}' | jq_ "d['id']")
api POST /v1/storage/buckets '{"name":"assets"}' >/dev/null
NOSQL=$(api POST /v1/nosql/databases '{"name":"events"}' | jq_ "d['id']")
api POST /v1/queues/vhosts '{"name":"jobs"}' >/dev/null
STREAM=$(api POST /v1/streams '{"name":"clicks"}' | jq_ "d['id']")
PG=$(api POST /v1/projects '{"name":"shop-db"}' | jq_ "d['id']")
# A real-looking row in the parent, which previews mask.
PG_PARENT_EP=$(api GET "/v1/projects/$PG" | jq_ "[b['endpoint']['id'] for b in d['branches'] if b['parent_id'] is None][0]")
api POST "/v1/endpoints/$PG_PARENT_EP/query" '{"sql":"CREATE TABLE users (id int PRIMARY KEY, email text); INSERT INTO users VALUES (1, '"'"'ada@example.com'"'"')"}' >/dev/null
api POST "/v1/streams/$STREAM/topics" '{"name":"clicks","partitions":3}' >/dev/null
api POST /_roll/tenants '{"id":"work"}' >/dev/null
# Data in the parents, so the branches have something to share: 20 MiB of objects, 20,000 keys.
KEY=$(api POST /v1/storage/keys '{"name":"e2e","permission":"admin"}')
export AWS_ACCESS_KEY_ID=$(echo "$KEY" | jq_ "d['id']") AWS_SECRET_ACCESS_KEY=$(echo "$KEY" | jq_ "d['secret_access_key']") AWS_DEFAULT_REGION=us-east-1
S3="aws --endpoint-url http://127.0.0.1:${P}900 s3"
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
for r in "repo/acme%2Fweb" "postgres/$PG" "redis/$REDIS" "bucket/assets" "nosql/$NOSQL" "queue/jobs" "stream/$STREAM" "tasks/work"; do
  api PUT "/v1/workspace/projects/$PRJ/resources/$r" '{"x":0,"y":0}' >/dev/null
done
api PUT "/v1/previews/projects/$PRJ/settings" "{\"enabled\":true,\"sleep_after_s\":60,\"release\":{\"image\":\"$IMAGE\",\"run\":\"true\"},\"masking\":{\"nosql:$NOSQL\":{\"app.users.email\":\"mask_default()\"},\"postgres:$PG\":{\"public.users.email\":\"email\"}}}" >/dev/null

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
# What CI outside GitHub does: wait for the change's preview, take the URL it prints.
WAITED=$(ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview wait --change "$CHG" --timeout 60)
[ "$WAITED" = "$(echo "$P" | jq_ "d['url']")" ] || { echo "roll preview wait printed $WAITED"; exit 1; }
HOST=$(echo "$P" | jq_ "d['services'][0]['url'].split('//')[1].split(':')[0]")
QUERY=$(echo "$P" | jq_ "d['services'][0]['share_url'].split('/?')[1]")
JAR="$HOME_DIR/cookies"
# A private preview (link access, the default) answers 401 without its link.
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $HOST" "http://$API/")" = 401 ] || { echo "a preview without its link should be 401"; exit 1; }
say "preview $PV ready in $((T1 - T0)) ms: $(echo "$P" | jq_ "[(r['kind'], r['ref']) for r in d['resources']]")"

say "write into the preview's branches: 1 MiB of objects, 1,000 keys"
BRANCH_BUCKET=$(echo "$P" | jq_ "[r['ref'] for r in d['resources'] if r['kind']=='bucket'][0]")
BRANCH_REDIS=$(echo "$P" | jq_ "[r['ref'] for r in d['resources'] if r['kind']=='redis'][0]")
$S3 cp --quiet "$HOME_DIR/blobs/1.bin" "s3://$BRANCH_BUCKET/new/1.bin"
resp "$(api GET "/v1/redis/databases/$BRANCH_REDIS" | jq_ "d['redis_url']")" 1000 preview
PARENT_OBJECTS=$(api GET /v1/storage/buckets/assets | jq_ "d['bytes']")
PG_BRANCH=$(echo "$P" | jq_ "([r['ref'] for r in d['resources'] if r['kind']=='postgres'] or [''])[0]")
PG_PARENT_KB=$(du -sk "$HOME_DIR/data/$(api GET "/v1/projects/$PG" | jq_ "[b['id'] for b in d['branches'] if b['parent_id'] is None][0]")" | cut -f1)
PG_BRANCH_KB=$( [ -n "$PG_BRANCH" ] && du -sk "$HOME_DIR/data/$PG_BRANCH" | cut -f1 || echo 0)
BRANCH_OBJECTS=$(api GET "/v1/storage/buckets/$BRANCH_BUCKET" | jq_ "d['bytes']")
BRANCH_SEES=$($S3 ls --recursive "s3://$BRANCH_BUCKET/" | wc -l)
# The preview's NoSQL branch reads masked; the parent's policy is untouched.
BRANCH_NOSQL=$(echo "$P" | jq_ "[r['ref'] for r in d['resources'] if r['kind']=='nosql'][0]")
MASKED=$(api GET "/v1/nosql/databases/$BRANCH_NOSQL" | jq_ "d['masking'].get('app.users.email')")
PARENT_MASKED=$(api GET "/v1/nosql/databases/$NOSQL" | jq_ "len(d['masking'])")
# The preview's Postgres branch has the email rewritten; the parent's row is as it was.
PG_BRANCH_EP=$(api GET "/v1/projects/$PG" | jq_ "[b['endpoint']['id'] for b in d['branches'] if b['id']=='$PG_BRANCH'][0]")
PG_MASKED=$(api POST "/v1/endpoints/$PG_BRANCH_EP/query" '{"sql":"SELECT email FROM users WHERE id = 1"}' | jq_ "d['results'][0]['rows'][0][0]")
PG_PARENT=$(api POST "/v1/endpoints/$PG_PARENT_EP/query" '{"sql":"SELECT email FROM users WHERE id = 1"}' | jq_ "d['results'][0]['rows'][0][0]")
case "$PG_MASKED" in user-*@example.invalid) ;; *) echo "the Postgres branch should read masked, not $PG_MASKED"; exit 1;; esac
[ "$PG_PARENT" = "ada@example.com" ] || { echo "the parent changed: $PG_PARENT"; exit 1; }

say "visit it: cold start"
T2=$(now_ms)
BODY=$(curl -sS -L -c "$JAR" -b "$JAR" -H "Host: $HOST" "http://$API/?$QUERY" --max-time 120)
T3=$(now_ms)
echo "$BODY" | grep -q "preview of change 1" || { echo "unexpected body: $BODY"; tail -30 "$HOME_DIR/rolld.log"; exit 1; }
T4=$(now_ms); curl -sS -o /dev/null -b "$JAR" -H "Host: $HOST" "http://$API/"; T5=$(now_ms)
# The service's DATABASE_URL is the branch's own role, which can't read the WAL (where the pages
# from before the mask are) and isn't a superuser.
APP_DB=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt" | sed -n 's/^DATABASE_URL=//p' | sed 's#host.docker.internal#127.0.0.1#')
case "$APP_DB" in postgres://br_*) ;; *) echo "the service's DATABASE_URL should be the branch's own role, not ${APP_DB%%@*}…"; exit 1;; esac
APP_WAL=$(docker run --rm --name roll-previews-psqltest --network host postgres:18-alpine psql "$APP_DB" -Atc "SELECT length(pg_read_binary_file('pg_wal/' || pg_walfile_name(pg_current_wal_lsn())))" 2>&1 | head -1 || true)
case "$APP_WAL" in *"permission denied"*) ;; *) echo "the branch's role should be refused the WAL: $APP_WAL"; exit 1;; esac
APP_MASKED=$(docker run --rm --name roll-previews-psqltest --network host postgres:18-alpine psql "$APP_DB" -Atc "SELECT email FROM users WHERE id = 1" 2>&1 | head -1 || true)
# Logs are pushed: a request made while following shows up on the stream.
FOLLOW="$HOME_DIR/follow.txt"
curl -sN --max-time 6 "http://$API/v1/previews/$PV/logs?follow=1&lines=1" >"$FOLLOW" &
FPID=$!
sleep 2
curl -sS -o /dev/null -b "$JAR" -H "Host: $HOST" "http://$API/pushed-log-line"
wait "$FPID" || true
grep -q "event: service" "$FOLLOW" && grep -q "pushed-log-line" "$FOLLOW" || { echo "the followed log should carry the request:"; cat "$FOLLOW"; exit 1; }
# CI's way in: the project's bypass secret in a header (no cookie); a wrong one is refused.
BYPASS=$(api POST "/v1/previews/projects/$PRJ/bypass" | jq_ "d['secret']")
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $HOST" -H "x-roll-preview-bypass: $BYPASS" "http://$API/")" = 200 ] || { echo "the bypass secret should open the preview"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $HOST" -H "x-roll-preview-bypass: rpb_wrong" "http://$API/")" = 401 ] || { echo "a wrong bypass secret should be 401"; exit 1; }
CONTAINER="roll-previews-$(echo "$HOST" | cut -d. -f1)"
STATS=$(docker stats --no-stream --format '{{.MemUsage}} {{.CPUPerc}}' "$CONTAINER")
RSS1=$(ps -o rss= -p "$PID")
api POST "/v1/previews/$PV/refresh" >/dev/null; sleep 3
ACTIVE=$(api GET "/v1/previews/$PV" | jq_ "d['cost']")

say "a screenshot of the service (github.screenshot), once per head"
api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"screenshot":true}}' >/dev/null
for _ in $(seq 1 120); do
  [ "$(api GET "/v1/previews/$PV" | jq_ "d.get('screenshot_at') is not None")" = True ] && break
  sleep 0.5
done
curl -sS -o "$HOME_DIR/screenshot.png" "http://$API/v1/previews/$PV/screenshot"
SHOT=$(python3 -c "import sys; b=open(sys.argv[1],'rb').read(); print(f'{len(b)} bytes, PNG' if b[:4]==b'\x89PNG' else 'not a PNG')" "$HOME_DIR/screenshot.png")
case "$SHOT" in *PNG) ;; *) echo "the screenshot should be a PNG: $SHOT"; exit 1;; esac
# A public preview's change carries a signed link to it (the pull request comment's image),
# which opens without credentials; a changed signature doesn't.
api PUT "/v1/previews/projects/$PRJ/settings" '{"access":"public"}' >/dev/null
SHOT_URL=""
for _ in $(seq 1 120); do
  SHOT_URL=$(api GET "/v1/code/changes/$CHG" | jq_ "((d.get('preview') or {}).get('environment') or {}).get('screenshot_url') or ''")
  [ -n "$SHOT_URL" ] && break
  sleep 0.5
done
SHOT_PATH="/${SHOT_URL#*://*/}"
[ "$(curl -s -o /dev/null -w '%{content_type}' "http://$API$SHOT_PATH")" = image/png ] || {
  echo "the signed link should open the screenshot: $SHOT_URL"
  api GET "/v1/previews/$PV" | jq_ "{k: d[k] for k in ('state', 'error', 'head', 'screenshot_at', 'updated_at')}"
  api GET "/v1/previews/projects/$PRJ/settings" | jq_ "(d['access'], d['github'])"
  api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d[:8]]"
  exit 1
}
[ "$(curl -s -o /dev/null -w '%{http_code}' "http://$API${SHOT_PATH%?}0")" = 403 ] || { echo "a changed signature should be refused"; exit 1; }
api PUT "/v1/previews/projects/$PRJ/settings" '{"access":"link"}' >/dev/null

say "sleep it"
api POST "/v1/previews/$PV/sleep" >/dev/null
sleep 1
GONE=$(docker ps -q --filter "name=^$CONTAINER\$" | wc -l)
du_of() { du -sk "$HOME_DIR/$1" 2>/dev/null | cut -f1; }
OBJ_KB=$(du_of objects)
SLEEPING=$(api GET "/v1/previews/$PV" | jq_ "d['cost']")

say "migrate the preview's Postgres branch, push: the next head's schema changes name the table"
if [ -n "$PG_BRANCH" ]; then
  PG_EP=$(api GET "/v1/projects/$PG" | jq_ "[b['endpoint']['id'] for b in d['branches'] if b['id']=='$PG_BRANCH'][0]")
  api POST "/v1/endpoints/$PG_EP/query" '{"sql":"CREATE TABLE preview_only (id int PRIMARY KEY, note text NOT NULL DEFAULT '"'"'x'"'"')"}' >/dev/null
  # The preview's code tries to get the superuser to run it (a function shadowing one the schema
  # diff calls, pg_get_indexdef(oid), the database's search_path pointed at it): the diff runs as the branch's role
  # with only the catalog's names, so the role stays what it is.
  APP_DB2=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt" | sed -n 's/^DATABASE_URL=//p' | sed 's#host.docker.internal#127.0.0.1#')
  APP_ROLE=$(echo "$APP_DB2" | sed -E 's#^postgres://([^:]+):.*#\1#')
  PLANTED=$(docker run --rm --name roll-previews-psqltest --network host postgres:18-alpine psql "$APP_DB2" -Atc "CREATE FUNCTION public.pg_get_indexdef(oid) RETURNS text LANGUAGE plpgsql AS \$f\$ BEGIN EXECUTE 'ALTER ROLE $APP_ROLE SUPERUSER'; RETURN 'x'; END \$f\$; ALTER DATABASE main SET search_path = public, pg_catalog" 2>&1 | tail -1 || true)
  [ "$PLANTED" = "ALTER DATABASE" ] || { echo "the hostile function should have been planted: $PLANTED"; exit 1; }
  api POST "/v1/code/changes/$CHG/revise" '{"base":"main","message":"New page, again","files":{"index.html":{"content":"preview of change 1, again"}}}' >/dev/null
  SCHEMA=""
  for _ in $(seq 1 120); do
    SCHEMA=$(api GET "/v1/previews/$PV" | jq_ "'\\n'.join(d.get('schema_changes') or [])")
    case "$SCHEMA" in *preview_only*) break;; esac
    sleep 0.5
  done
  case "$SCHEMA" in *"+ table public.preview_only"*) ;; *) echo "the schema changes should name the new table: $SCHEMA"; exit 1;; esac
  ESCALATED=$(api POST "/v1/endpoints/$PG_EP/query" "{\"sql\":\"SELECT rolsuper FROM pg_roles WHERE rolname = '$APP_ROLE'\"}" | jq_ "d['results'][0]['rows'][0][0]")
  [ "$ESCALATED" = f ] || { echo "the branch's role became a superuser ($ESCALATED)"; exit 1; }
  say "$SCHEMA"
fi

say "a preview's own variable reaches its running service; roll's names are refused"
ROLL_CLI="$ROOT/target/debug/roll"
ROLL_API_URL="http://$API" "$ROLL_CLI" preview env "$PV" FEATURE_FLAG=on >/dev/null
if ROLL_API_URL="http://$API" "$ROLL_CLI" preview env "$PV" DATABASE_URL=postgres://elsewhere >/dev/null 2>&1; then echo "DATABASE_URL should be refused as a preview variable"; exit 1; fi
VAR_SEEN=""
for _ in $(seq 1 120); do
  VAR_SEEN=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt" | sed -n 's/^FEATURE_FLAG=//p')
  [ "$VAR_SEEN" = on ] && break
  sleep 0.5
done
[ "$VAR_SEEN" = on ] || { echo "the service should see FEATURE_FLAG=on after roll preview env"; api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d[:8]]"; exit 1; }

say "with ignore_paths, a docs-only push keeps the services and the preview reports the new head"
api PUT "/v1/previews/projects/$PRJ/settings" '{"ignore_paths":["docs/"]}' >/dev/null
bare() { echo "${1#cs_}"; }
until_head() { # until_head <change head>: the preview is for it
  for _ in $(seq 1 120); do
    [ "$(bare "$(api GET "/v1/previews/$PV" | jq_ "d['head']")")" = "$(bare "$1")" ] && return 0
    sleep 0.5
  done
  echo "the preview should follow the change to $1"; api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d[:8]]"; exit 1
}
BUILD=$(api GET "/v1/previews/$PV" | jq_ "d['head']")
api POST "/v1/code/changes/$CHG/revise" '{"base":"main","message":"Docs","files":{"index.html":{"content":"preview of change 1, again"},"docs/setup.md":{"content":"how to run it"}}}' >/dev/null
DOCS_HEAD=$(api GET "/v1/code/changes/$CHG" | jq_ "d['head']")
until_head "$DOCS_HEAD"
RUNS=$(api GET "/v1/previews/$PV" | jq_ "d.get('runs') or ''")
[ "$(bare "$RUNS")" = "$(bare "$BUILD")" ] || { echo "a docs-only push should keep the services on $BUILD, not $RUNS"; exit 1; }
WAITED2=$(ROLL_API_URL="http://$API" "$ROLL_CLI" preview wait --change "$CHG" --timeout 30) || { echo "roll preview wait should pass for the docs-only head"; exit 1; }
api POST "/v1/code/changes/$CHG/revise" '{"base":"main","message":"Code","files":{"index.html":{"content":"preview of change 1, third"},"docs/setup.md":{"content":"how to run it"}}}' >/dev/null
CODE_HEAD=$(api GET "/v1/code/changes/$CHG" | jq_ "d['head']")
until_head "$CODE_HEAD"
for _ in $(seq 1 120); do
  [ "$(api GET "/v1/previews/$PV" | jq_ "not d.get('runs') and d['state'] in ('ready', 'sleeping')")" = True ] && break
  sleep 0.5
done
PAGE=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/")
[ "$PAGE" = "preview of change 1, third" ] || { echo "a push that touches code should redeploy: the page says $PAGE"; exit 1; }
KEPT="kept $(bare "$RUNS" | cut -c1-10) through a docs-only push; redeployed at $(bare "$CODE_HEAD" | cut -c1-10)"

say "[skip preview] in the title removes the preview; without it, a new one comes"
api PATCH "/v1/code/changes/$CHG" '{"title":"New page [skip preview]"}' >/dev/null
for _ in $(seq 1 120); do
  N=$(api GET "/v1/previews?project=$PRJ" | jq_ "len(d)")
  [ "$N" = 0 ] && break
  sleep 0.5
done
[ "$N" = 0 ] || { echo "[skip preview] should remove the preview"; exit 1; }
api PATCH "/v1/code/changes/$CHG" '{"title":"New page"}' >/dev/null
for _ in $(seq 1 240); do
  PV=$(api GET "/v1/previews?project=$PRJ" | jq_ "d[0]['id'] if d and d[0]['state'] in ('ready','sleeping') else ''")
  [ -n "$PV" ] && break
  sleep 0.5
done
[ -n "$PV" ] || { echo "a new preview should come once the title stops skipping it"; exit 1; }

if [ -n "${STOP_BEFORE_LAND:-}" ]; then
  echo "stopped before landing: project $PRJ, preview $PV (run with --keep to leave the daemon up)"
  exit 0
fi
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
| NoSQL branch's mask on app.users.email / masks on the parent | $MASKED / $PARENT_MASKED |
| Postgres users.email in the branch / in the parent | $PG_MASKED / $PG_PARENT |
| The service's Postgres role / its WAL read / users.email as it reads it | $(echo "$APP_DB" | sed -E 's#^postgres://([^:]+):.*#\1#') / refused / $APP_MASKED |
| Postgres: the parent's data directory / the branch's (a full copy on this disk) | ${PG_PARENT_KB} KiB / ${PG_BRANCH_KB} KiB |
| First visit (cold start of the service container, image cached) | $((T3 - T2)) ms |
| Next visit (warm) | $((T5 - T4)) ms |
| Service container while running | $STATS |
| rolld RSS before → with the preview running | $((RSS0 / 1024)) MiB → $((RSS1 / 1024)) MiB |
| Containers left after sleep | $GONE |
| Object storage of the whole daemon after the preview (parents included) | ${OBJ_KB} KiB |
| Cost while running (from the preview) | $ACTIVE |
| Cost while asleep (from the preview) | $SLEEPING |
| The first service's screenshot (taken in the service container's own network) | $SHOT |
| A preview's own variable (`roll preview env`) in the running service / `DATABASE_URL` as one | FEATURE_FLAG=$VAR_SEEN / refused |
| ignore_paths: docs/ | $KEPT |
| `[skip preview]` in the title, then without it | removed, then a new preview |
| After landing: parents present / preview Redis branches left | $PARENTS_OK of 3 / $BRANCHES_LEFT |
EOF
