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
  ROLL_SEARCH_HTTP=127.0.0.1:${P}200 \
  ROLL_NOSQL_CQL=127.0.0.1:${P}042 ROLL_NOSQL_CQL_SHARD_AWARE=127.0.0.1:${P}043 ROLL_NOSQL_ALTERNATOR=127.0.0.1:${P}000 ROLL_NOSQL_REST=127.0.0.1:${P}100 \
  ROLL_CODE_PREVIEW_DOMAIN=$DOMAIN ROLL_PRODUCTS=${PREVIEW_PRODUCTS:-code,postgres,redis,storage,nosql,queues,streams,tasks,search} RUST_LOG=${RUST_LOG:-info}
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
TOML='[preview]\nimage = \"'"$IMAGE"'\"\nrun = \"env > env.txt && python -m http.server $PORT\"\nport = 8000\nhealth = \"/\"\nmemory = 128\ncpus = 0.25\n'
api POST /v1/code/repos/acme/web/land "{\"message\":\"init\",\"files\":{\"roll.toml\":{\"content\":\"$TOML\"},\"index.html\":{\"content\":\"main\"}}}" >/dev/null
# A second service, from a second repo (its default branch in every preview): its pages too.
api POST /v1/code/repos '{"owner":"acme","name":"wiki","source":"native"}' >/dev/null
api POST /v1/code/repos/acme/wiki/land "{\"message\":\"init\",\"files\":{\"roll.toml\":{\"content\":\"$TOML\"},\"index.html\":{\"content\":\"the wiki\"}}}" >/dev/null
REDIS=$(api POST /v1/redis/databases '{"name":"cache"}' | jq_ "d['id']")
api POST /v1/storage/buckets '{"name":"assets"}' >/dev/null
NOSQL=$(api POST /v1/nosql/databases '{"name":"events"}' | jq_ "d['id']")
CQL() { api POST "/v1/nosql/databases/$1/cql" "$(python3 -c 'import json,sys; print(json.dumps({"query": sys.argv[1]}))' "$2")"; }
CQL "$NOSQL" "CREATE KEYSPACE app WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}" >/dev/null
CQL "$NOSQL" "CREATE TABLE app.users (id int PRIMARY KEY, email text, name text)" >/dev/null
CQL "$NOSQL" "INSERT INTO app.users (id, email, name) VALUES (1, 'alice@corp.example', 'Alice')" >/dev/null
CQL "$NOSQL" "CREATE TABLE app.users_by_email (email text PRIMARY KEY, id int)" >/dev/null
api POST /v1/queues/vhosts '{"name":"jobs"}' >/dev/null
STREAM=$(api POST /v1/streams '{"name":"clicks"}' | jq_ "d['id']")
# A search cluster with a document the preview's branch must read.
SEARCH_C=$(api POST /v1/search/clusters '{"name":"catalog"}')
SEARCH=$(echo "$SEARCH_C" | jq_ "d['id']")
SEARCH_URL_PARENT=$(echo "$SEARCH_C" | jq_ "d['url'].replace('://', '://' + d['username'] + ':' + d['password'] + '@', 1)")
curl -sf -XPUT "$SEARCH_URL_PARENT/products/_doc/1?refresh=true" -H 'content-type: application/json' -d '{"name":"parent"}' >/dev/null
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
for r in "repo/acme%2Fweb" "repo/acme%2Fwiki" "postgres/$PG" "redis/$REDIS" "bucket/assets" "nosql/$NOSQL" "queue/jobs" "stream/$STREAM" "tasks/work" "search/$SEARCH"; do
  api PUT "/v1/workspace/projects/$PRJ/resources/$r" '{"x":0,"y":0}' >/dev/null
done
api PUT "/v1/previews/projects/$PRJ/settings" "{\"enabled\":true,\"sleep_after_s\":60,\"release\":{\"image\":\"$IMAGE\",\"run\":\"true\"},\"masking\":{\"nosql:$NOSQL\":{\"app.users.email\":\"mask_default()\"},\"postgres:$PG\":{\"public.users.email\":\"email\"},\"search:$SEARCH\":{\"products.name\":\"redact\"}}}" >/dev/null
# A mask on a key column can't hide it (the engine refuses to read such a table): the settings
# name it instead of letting a preview's pages fail.
KEY_MASK=$(curl -s -o /tmp/roll-previews-keymask.json -w '%{http_code}' -X PUT "http://$API/v1/previews/projects/$PRJ/settings" -H 'content-type: application/json' \
  -d "{\"masking\":{\"nosql:$NOSQL\":{\"app.users.email\":\"mask_default()\",\"app.users_by_email.email\":\"mask_default()\"}}}")
[ "$KEY_MASK" = 400 ] && grep -q "app.users_by_email's primary key" /tmp/roll-previews-keymask.json || { echo "a NoSQL mask on a key column should be refused, not $KEY_MASK $(cat /tmp/roll-previews-keymask.json)"; exit 1; }

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
# Ready only once its health path answered (roll.toml `health = "/"`).
api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d]" | grep -q "is healthy (/: 200)" || { echo "the service's health check should have passed before ready"; exit 1; }
# What CI outside GitHub does: wait for the change's preview, take the URL it prints.
WAITED=$(ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview wait --change "$CHG" --timeout 60)
[ "$WAITED" = "$(echo "$P" | jq_ "d['url']")" ] || { echo "roll preview wait printed $WAITED"; exit 1; }
HOST=$(echo "$P" | jq_ "[x for x in d['services'] if x['name'] == 'web'][0]['url'].split('//')[1].split(':')[0]")
QUERY=$(echo "$P" | jq_ "[x for x in d['services'] if x['name'] == 'web'][0]['share_url'].split('/?')[1]")
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
# What each branch hands the preview's code: masked where masks are set, none where nothing is
# copied, and production data as it is everywhere else, which the preview says.
DATA=$(echo "$P" | jq_ "' '.join(sorted('%s=%s' % (r['kind'], r['data']) for r in d['resources']))")
[ "$DATA" = "bucket=real nosql=masked postgres=masked queue=none redis=real search=masked stream=real tasks=none" ] || { echo "the preview should say what data each branch hands over: $DATA"; exit 1; }
# How long the release (migrations) took on the preview's data, at the head it runs; the
# change carries it too (its page and the pull request comment).
RELEASE=$(echo "$P" | jq_ "'%s ms at %s' % (d['release']['ms'], d['release']['head'] == d['head']) if d.get('release') else ''")
case "$RELEASE" in [0-9]*" ms at True") ;; *) echo "the preview should say how long its release took at its head: $RELEASE"; exit 1;; esac
for _ in $(seq 1 60); do
  CHG_RELEASE=$(api GET "/v1/code/changes/$CHG" | jq_ "((d.get('preview') or {}).get('environment') or {}).get('release_ms')")
  case "$CHG_RELEASE" in [0-9]*) break;; esac
  sleep 0.5
done
case "$CHG_RELEASE" in [0-9]*) ;; *) echo "the change should carry the release's time: $CHG_RELEASE"; exit 1;; esac
# Its link with access as a QR code, for a phone (a PNG, never cached: it carries the token).
QR=$(curl -sS -D "$HOME_DIR/qr.h" "http://$API/v1/previews/$PV/qr.png" | head -c 8 | od -An -c | tr -d ' \n')
grep -qi '^cache-control: no-store' "$HOME_DIR/qr.h" && [ "$QR" = '211PNG\r\n032\n' ] || { echo "the preview's link should come as a QR code PNG, never cached: $QR"; exit 1; }
# The preview's NoSQL branch reads masked; the parent's policy is untouched.
BRANCH_NOSQL=$(echo "$P" | jq_ "[r['ref'] for r in d['resources'] if r['kind']=='nosql'][0]")
MASKED=$(api GET "/v1/nosql/databases/$BRANCH_NOSQL" | jq_ "d['masking'].get('app.users.email')")
PARENT_MASKED=$(api GET "/v1/nosql/databases/$NOSQL" | jq_ "len(d['masking'])")
# What the preview's own code can ask its branch (the console is its superuser): the email reads
# masked, and a filter on the real address finds nothing (the premise: on the parent it does).
[ "$(CQL "$NOSQL" "SELECT name FROM app.users WHERE email = 'alice@corp.example' ALLOW FILTERING" | jq_ "d['rows']")" = "[['Alice']]" ] || { echo "the parent's filter should find Alice"; exit 1; }
NOSQL_READ=$(CQL "$BRANCH_NOSQL" "SELECT email FROM app.users WHERE id = 1" | jq_ "d['rows'][0][0]")
NOSQL_FILTER=$(CQL "$BRANCH_NOSQL" "SELECT name FROM app.users WHERE email = 'alice@corp.example' ALLOW FILTERING" | jq_ "d.get('rows', d)")
[ "$NOSQL_READ" != "alice@corp.example" ] && [ "$NOSQL_FILTER" = "[]" ] || { echo "the NoSQL branch tells the masked email: read $NOSQL_READ, filter $NOSQL_FILTER"; exit 1; }
# The preview's Postgres branch has the email rewritten; the parent's row is as it was.
PG_BRANCH_EP=$(api GET "/v1/projects/$PG" | jq_ "[b['endpoint']['id'] for b in d['branches'] if b['id']=='$PG_BRANCH'][0]")
PG_MASKED=$(api POST "/v1/endpoints/$PG_BRANCH_EP/query" '{"sql":"SELECT email FROM users WHERE id = 1"}' | jq_ "d['results'][0]['rows'][0][0]")
PG_PARENT=$(api POST "/v1/endpoints/$PG_PARENT_EP/query" '{"sql":"SELECT email FROM users WHERE id = 1"}' | jq_ "d['results'][0]['rows'][0][0]")
case "$PG_MASKED" in user-*@example.invalid) ;; *) echo "the Postgres branch should read masked, not $PG_MASKED"; exit 1;; esac
[ "$PG_PARENT" = "ada@example.com" ] || { echo "the parent changed: $PG_PARENT"; exit 1; }
# The search branch reads the parent's document, and its own writes stay in it.
BRANCH_SEARCH=$(echo "$P" | jq_ "[r['ref'] for r in d['resources'] if r['kind']=='search'][0]")
[ -n "$BRANCH_SEARCH" ] || { echo "no search branch in the preview"; exit 1; }

say "visit it: cold start"
T2=$(now_ms)
BODY=$(curl -sS -L -c "$JAR" -b "$JAR" -H "Host: $HOST" "http://$API/?$QUERY" --max-time 120)
T3=$(now_ms)
echo "$BODY" | grep -q "preview of change 1" || { echo "unexpected body: $BODY"; tail -30 "$HOME_DIR/rolld.log"; exit 1; }
T4=$(now_ms); curl -sS -o /dev/null -b "$JAR" -H "Host: $HOST" "http://$API/"; T5=$(now_ms)
# Never indexed: the service's page through roll says so, and so does the 401 without the link.
ROBOTS=$(curl -sS -o /dev/null -D - -b "$JAR" -H "Host: $HOST" "http://$API/" | tr -d '\r' | sed -n 's/^x-robots-tag: //Ip')
ROBOTS_LOCKED=$(curl -sS -o /dev/null -D - -H "Host: $HOST" "http://$API/" | tr -d '\r' | sed -n 's/^x-robots-tag: //Ip')
[ "$ROBOTS" = noindex ] && [ "$ROBOTS_LOCKED" = noindex ] || { echo "a preview's pages should be kept out of search engines: '$ROBOTS' / '$ROBOTS_LOCKED'"; exit 1; }
# The service's DATABASE_URL is the branch's own role, which can't read the WAL (where the pages
# from before the mask are) and isn't a superuser.
APP_DB=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt" | sed -n 's/^DATABASE_URL=//p' | sed 's#host.docker.internal#127.0.0.1#')
case "$APP_DB" in postgres://br_*) ;; *) echo "the service's DATABASE_URL should be the branch's own role, not ${APP_DB%%@*}…"; exit 1;; esac
# The service's ELASTICSEARCH_URL is the branch's: it reads the parent's document; a write stays in
# the branch.
APP_ES=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt" | sed -n 's/^ELASTICSEARCH_URL=//p' | sed 's#host.docker.internal#127.0.0.1#')
[ -n "$APP_ES" ] || { echo "the service has no ELASTICSEARCH_URL"; exit 1; }
# products.name is masked in the branch (the project's masking), and only there.
ES_READ=$(curl -sf "$APP_ES/products/_doc/1" | jq_ "d['_source']['name']")
[ "$ES_READ" = "[masked]" ] || { echo "the search branch should read the parent's document with products.name masked, not $ES_READ"; exit 1; }
ES_PARENT=$(curl -sf "$SEARCH_URL_PARENT/products/_doc/1" | jq_ "d['_source']['name']")
[ "$ES_PARENT" = "parent" ] || { echo "masking the branch shouldn't touch the parent's document: $ES_PARENT"; exit 1; }
# Nor can the preview's code read the masked value back out of the segments the branch shares
# with its parent (a term dictionary keeps a replaced document's terms): the parent's has it,
# the branch's doesn't.
TERMS() { curl -sf -XPOST "$1/products/_terms_enum" -H 'content-type: application/json' -d '{"field":"name.keyword","string":"par"}' | jq_ "d.get('terms')"; }
[ "$(TERMS "$SEARCH_URL_PARENT")" = "['parent']" ] || { echo "the parent's term dictionary should have the value (the check's premise): $(TERMS "$SEARCH_URL_PARENT")"; exit 1; }
ES_TERMS=$(TERMS "$APP_ES")
[ "$ES_TERMS" = "[]" ] || { echo "the masked value is still in the branch's term dictionary: $ES_TERMS"; exit 1; }
# A field only the branch's documents have (price): its mapping gains it, and the next head's
# schema changes say so.
curl -sf -XPUT "$APP_ES/products/_doc/2?refresh=true" -H 'content-type: application/json' -d '{"name":"branch","price":9.5}' >/dev/null
ES_PARENT_SEES=$(curl -s -o /dev/null -w '%{http_code}' "$SEARCH_URL_PARENT/products/_doc/2")
[ "$ES_PARENT_SEES" = 404 ] || { echo "a write to the search branch reached the parent ($ES_PARENT_SEES)"; exit 1; }
APP_WAL=$(docker run --rm --name roll-previews-psqltest --network host postgres:18-alpine psql "$APP_DB" -Atc "SELECT length(pg_read_binary_file('pg_wal/' || pg_walfile_name(pg_current_wal_lsn())))" 2>&1 | head -1 || true)
case "$APP_WAL" in *"permission denied"*) ;; *) echo "the branch's role should be refused the WAL: $APP_WAL"; exit 1;; esac
APP_MASKED=$(docker run --rm --name roll-previews-psqltest --network host postgres:18-alpine psql "$APP_DB" -Atc "SELECT email FROM users WHERE id = 1" 2>&1 | head -1 || true)
# Logs are pushed: a request made while following shows up on the stream.
FOLLOW="$HOME_DIR/follow.txt"
curl -sN --max-time 20 "http://$API/v1/previews/$PV/logs?follow=1&lines=1" >"$FOLLOW" &
FPID=$!
# Once the stream is open (its first event), a request; then until its line arrives.
for _ in $(seq 1 40); do grep -q "event: service" "$FOLLOW" 2>/dev/null && break; sleep 0.25; done
curl -sS -o /dev/null -b "$JAR" -H "Host: $HOST" "http://$API/pushed-log-line"
for _ in $(seq 1 60); do grep -q "pushed-log-line" "$FOLLOW" && break; sleep 0.25; done
kill "$FPID" 2>/dev/null || true
wait "$FPID" 2>/dev/null || true
grep -q "event: service" "$FOLLOW" && grep -q "pushed-log-line" "$FOLLOW" || { echo "the followed log should carry the request:"; cat "$FOLLOW"; exit 1; }
# CI's way in: the project's bypass secret in a header (no cookie); a wrong one is refused.
BYPASS=$(api POST "/v1/previews/projects/$PRJ/bypass" | jq_ "d['secret']")
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $HOST" -H "x-roll-preview-bypass: $BYPASS" "http://$API/")" = 200 ] || { echo "the bypass secret should open the preview"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -H "Host: $HOST" -H "x-roll-preview-bypass: rpb_wrong" "http://$API/")" = 401 ] || { echo "a wrong bypass secret should be 401"; exit 1; }
# The feedback button: on the page a visitor through roll opens, and its script from the same host.
curl -sS -b "$JAR" -H "Host: $HOST" -H "Accept: text/html" "http://$API/" | grep -q '/__roll/feedback.js' || { echo "the page should carry the feedback button"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{content_type}' -b "$JAR" -H "Host: $HOST" "http://$API/__roll/feedback.js")" = "text/javascript; charset=utf-8" ] || { echo "the feedback script should load from the preview's host"; exit 1; }
# A note posted from the button (a comment review in the button's shape) is listed on its page.
NOTE_BODY='**Feedback on the preview** · [`/`](http://x/) · `body > h1`\n\nThe heading is cut off\n\n<sub>roll preview</sub>'
api POST "/v1/code/changes/$CHG/reviews" "{\"verdict\":\"comment\",\"body\":\"$NOTE_BODY\"}" >/dev/null
NOTES=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/__roll/feedback.json?path=%2F")
echo "$NOTES" | grep -q "The heading is cut off" || { echo "the page's notes should list the feedback: $NOTES"; exit 1; }
# A share link for someone outside the team: in through its own token (no preview cookie), out
# once it's revoked.
SHARE=$(api POST "/v1/previews/$PV/shares" '{"hours":1}')
SHARE_ID=$(echo "$SHARE" | jq_ "d['id']")
SHARE_Q=$(echo "$SHARE" | jq_ "d['url'].split('/?')[1]")
SJAR="$HOME_DIR/cookies-share"
[ "$(curl -s -o /dev/null -w '%{http_code}' -c "$SJAR" -H "Host: $HOST" "http://$API/?$SHARE_Q")" = 302 ] || { echo "a share link should let its visitor in"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -b "$SJAR" -H "Host: $HOST" "http://$API/")" = 200 ] || { echo "a share link's cookie should open the preview"; exit 1; }
api DELETE "/v1/previews/$PV/shares/$SHARE_ID" >/dev/null
[ "$(curl -s -o /dev/null -w '%{http_code}' -b "$SJAR" -H "Host: $HOST" "http://$API/")" = 401 ] || { echo "a revoked share link should stop at once"; exit 1; }
CONTAINER="roll-previews-$(echo "$HOST" | cut -d. -f1)"
STATS=$(docker stats --no-stream --format '{{.MemUsage}} {{.CPUPerc}}' "$CONTAINER")
RSS1=$(ps -o rss= -p "$PID")
api POST "/v1/previews/$PV/refresh" >/dev/null; sleep 3
ACTIVE=$(api GET "/v1/previews/$PV" | jq_ "d['cost']")

say "a screenshot of the service (github.screenshot), once per head"
# And /pricing, which the test service doesn't have (a 404 page, one failed request).
api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"screenshot":true,"pages":["/pricing","web:/","wiki:/"],"hide":[".clock"],"budget":{"load_ms":60000}}}' >/dev/null
# roll keeps a preview of the default branch (github.baseline): the change's screenshots are
# compared with its.
MAINPV=""
for _ in $(seq 1 120); do
  MAINPV=$(api GET "/v1/previews?project=$PRJ" | jq_ "next((p['id'] for p in d if p.get('branch') == 'main' and p['created_by'] == 'roll'), '')")
  [ -n "$MAINPV" ] && break
  sleep 0.5
done
[ -n "$MAINPV" ] || { echo "roll should keep a preview of main once screenshots are on (github.baseline)"; exit 1; }
for _ in $(seq 1 120); do
  [ "$(api GET "/v1/previews/$PV" | jq_ "d.get('screenshot_at') is not None")" = True ] && break
  sleep 0.5
done
curl -sS -o "$HOME_DIR/screenshot.png" "http://$API/v1/previews/$PV/screenshot"
SHOT=$(python3 -c "import sys; b=open(sys.argv[1],'rb').read(); print(f'{len(b)} bytes, PNG' if b[:4]==b'\x89PNG' else 'not a PNG')" "$HOME_DIR/screenshot.png")
case "$SHOT" in *PNG) ;; *) echo "the screenshot should be a PNG: $SHOT"; exit 1;; esac
# The same load timed the page (shot.js over the DevTools pipe).
SPEED=$(api GET "/v1/previews/$PV" | jq_ "('load %s ms, first byte %s ms, %s bytes' % (d['page_speed']['now']['load_ms'], d['page_speed']['now']['ttfb_ms'], d['page_speed']['now']['bytes'])) if d.get('page_speed') and d['page_speed']['now']['load_ms'] > 0 else ''")
[ -n "$SPEED" ] || { echo "the screenshot's load should have timed the page"; api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d[:6]]"; exit 1; }
# Core Web Vitals from the same load: the page's text is its largest paint, and nothing shifts.
VITALS=$(api GET "/v1/previews/$PV" | jq_ "'LCP %s ms, CLS %s' % (d['page_speed']['now'].get('lcp_ms'), d['page_speed']['now'].get('cls'))")
case "$VITALS" in "LCP "[1-9]*" ms, CLS 0"*) ;; *) echo "the screenshot's load should measure Core Web Vitals: $VITALS"; exit 1;; esac
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
# Its signature's last character changed (to one it isn't): refused.
case "$SHOT_PATH" in *0) TAMPERED="${SHOT_PATH%?}1";; *) TAMPERED="${SHOT_PATH%?}0";; esac
[ "$(curl -s -o /dev/null -w '%{http_code}' "http://$API$TAMPERED")" = 403 ] || { echo "a changed signature should be refused"; exit 1; }
api PUT "/v1/previews/projects/$PRJ/settings" '{"access":"link"}' >/dev/null

say "sleep it"
api POST "/v1/previews/$PV/sleep" >/dev/null
for _ in $(seq 1 60); do
  [ "$(docker ps -q --filter "name=^$CONTAINER\$" | wc -l)" = 0 ] && break
  sleep 0.5
done
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
  # Asleep, it wakes on the visit (a 503 waking page until then, as a browser's refresh would).
  for _ in $(seq 1 60); do
    ENV_TXT=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt")
    case "$ENV_TXT" in *DATABASE_URL=*) break;; esac
    sleep 1
  done
  APP_DB2=$(echo "$ENV_TXT" | sed -n 's/^DATABASE_URL=//p' | sed 's#host.docker.internal#127.0.0.1#')
  APP_ROLE=$(echo "$APP_DB2" | sed -E 's#^postgres://([^:]+):.*#\1#')
  PLANTED=$(docker run --rm --name roll-previews-psqltest --network host postgres:18-alpine psql "$APP_DB2" -Atc "CREATE FUNCTION public.pg_get_indexdef(oid) RETURNS text LANGUAGE plpgsql AS \$f\$ BEGIN EXECUTE 'ALTER ROLE $APP_ROLE SUPERUSER'; RETURN 'x'; END \$f\$; ALTER DATABASE main SET search_path = public, pg_catalog" 2>&1 | tail -1 || true)
  [ "$PLANTED" = "ALTER DATABASE" ] || { echo "the hostile function should have been planted: $PLANTED (env.txt: $(echo "$ENV_TXT" | head -c 300))"; exit 1; }
  api POST "/v1/code/changes/$CHG/revise" '{"base":"main","message":"New page, again","files":{"index.html":{"content":"preview of change 1, again"}}}' >/dev/null
  SCHEMA=""
  for _ in $(seq 1 120); do
    SCHEMA=$(api GET "/v1/previews/$PV" | jq_ "'\\n'.join(d.get('schema_changes') or [])")
    case "$SCHEMA" in *preview_only*) break;; esac
    sleep 0.5
  done
  case "$SCHEMA" in *"+ table public.preview_only"*) ;; *) echo "the schema changes should name the new table: $SCHEMA"; exit 1;; esac
  case "$SCHEMA" in *"+ field products.price "*) ;; *) echo "the schema changes should name the search branch's new field: $SCHEMA"; exit 1;; esac
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
[ "$VAR_SEEN" = on ] || {
  echo "the service should see FEATURE_FLAG=on after roll preview env"
  api GET "/v1/previews/$PV" | jq_ "{k: d[k] for k in ('state', 'error', 'head', 'updated_at')}"
  api GET "/v1/previews/$PV/activity" | jq_ "[(a['at'] % 100000, a['message']) for a in d[:16]]"
  curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt" | grep -c . || true
  exit 1
}

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
# This one's page logs an error, which its screenshot's load must catch.
THIRD='preview of change 1, third<script>console.error(\"boom\")</script>'
api POST "/v1/code/changes/$CHG/revise" "{\"base\":\"main\",\"message\":\"Code\",\"files\":{\"index.html\":{\"content\":\"$THIRD\"},\"docs/setup.md\":{\"content\":\"how to run it\"}}}" >/dev/null
CODE_HEAD=$(api GET "/v1/code/changes/$CHG" | jq_ "d['head']")
until_head "$CODE_HEAD"
for _ in $(seq 1 120); do
  [ "$(api GET "/v1/previews/$PV" | jq_ "not d.get('runs') and d['state'] in ('ready', 'sleeping')")" = True ] && break
  sleep 0.5
done
# Warm after the push: its container is up before anyone visits.
WARM=no
for _ in $(seq 1 60); do
  [ "$(docker ps -q --filter "name=^$CONTAINER\$" | wc -l)" = 1 ] && { WARM=yes; break; }
  sleep 0.5
done
[ "$WARM" = yes ] || { echo "the service should be woken after the push, before any visit"; exit 1; }
PAGE=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/")
[ "$PAGE" = 'preview of change 1, third<script>console.error("boom")</script>' ] || { echo "a push that touches code should redeploy: the page says $PAGE"; exit 1; }
KEPT="kept $(bare "$RUNS" | cut -c1-10) through a docs-only push; redeployed at $(bare "$CODE_HEAD" | cut -c1-10)"
# The push before stays reachable at a URL of its own, on the same data (Vercel's per-push URLs).
EARLIER_URL=$(api GET "/v1/previews/$PV" | jq_ "((d.get('earlier') or [{}])[0]).get('share_url') or ''")
[ -n "$EARLIER_URL" ] || { echo "the previous push should stay reachable"; api GET "/v1/previews/$PV" | jq_ "d.get('earlier')"; exit 1; }
EARLIER_HOST=$(echo "$EARLIER_URL" | sed -E 's#^https?://([^:/]+).*#\1#')
EJAR="$HOME_DIR/cookies-earlier"
curl -sS -o /dev/null -c "$EJAR" -H "Host: $EARLIER_HOST" "http://$API/?${EARLIER_URL#*\?}"
EARLIER_PAGE=""
for _ in $(seq 1 120); do
  EARLIER_PAGE=$(curl -sS -b "$EJAR" -H "Host: $EARLIER_HOST" "http://$API/")
  [ "$EARLIER_PAGE" = "preview of change 1, again" ] && break
  sleep 0.5
done
[ "$EARLIER_PAGE" = "preview of change 1, again" ] || { echo "the earlier push should serve its own page: $EARLIER_PAGE"; exit 1; }

# Screenshots stayed on through the pushes: the page's change since the head before is known.
VISUAL=""
for _ in $(seq 1 120); do
  VISUAL=$(api GET "/v1/previews/$PV" | jq_ "('%s%% since %s' % (d['visual_change']['percent'], d['visual_change']['since'][:10])) if d.get('visual_change') else ''")
  [ -n "$VISUAL" ] && break
  sleep 0.5
done
[ -n "$VISUAL" ] || { echo "the preview should say how much its page changed since the push before"; exit 1; }
# What went wrong while the page loaded: its console.error, caught by the screenshot's browser.
HEALTH=""
for _ in $(seq 1 120); do
  HEALTH=$(api GET "/v1/previews/$PV" | jq_ "('%s error(s), first: %s; %s failed' % (d['page_health']['errors'], d['page_health']['first_error'], d['page_health']['failed'])) if (d.get('page_health') or {}).get('errors') else ''")
  case "$HEALTH" in *boom*) break;; esac
  sleep 0.5
done
case "$HEALTH" in *boom*) ;; *) echo "the page's console.error should be caught: $(api GET "/v1/previews/$PV" | jq_ "d.get('page_health')")"; exit 1;; esac
# The test page has no language and no title: the screenshot's browser says so.
# By axe-core's rules (a bare text page breaks its landmark ones), with no lang and no title.
A11Y=$(api GET "/v1/previews/$PV" | jq_ "('lang %s, title %s, axe %s: %s' % (d['page_a11y']['lang'], d['page_a11y']['title'], any(i.get('impact') for i in d['page_a11y']['issues']), ' '.join(sorted(i['kind'] for i in d['page_a11y']['issues'])))) if d.get('page_a11y') else ''")
case "$A11Y" in "lang False, title False, axe True: "?*) ;; *) echo "the page's accessibility should be checked by axe-core (no lang, no title): $A11Y"; exit 1;; esac
[ "$(curl -s -o /dev/null -w '%{content_type}' "http://$API/v1/previews/$PV/screenshot?previous=1")" = image/png ] || { echo "the screenshot before should be there"; exit 1; }
# And at a phone's width, with its own change.
[ "$(api GET "/v1/previews/$PV" | jq_ "d.get('phone_screenshot')")" = True ] || { echo "a phone-width screenshot should be taken too"; exit 1; }
# Against the default branch's preview (Chromatic's baseline).
BASE=""
for _ in $(seq 1 120); do
  BASE=$(api GET "/v1/previews/$PV" | jq_ "('%s%% against %s' % (d['base_change']['percent'], d['base_change']['branch'])) if d.get('base_change') else ''")
  [ -n "$BASE" ] && break
  sleep 0.5
done
[ -n "$BASE" ] || { echo "the change's page should be compared with main's preview: $(api GET "/v1/previews/$MAINPV" | jq_ "{k: d.get(k) for k in ('state', 'screenshot_at', 'branch')}")"; exit 1; }
# At the change's merge base: main's head, which the baseline runs.
[ "$(api GET "/v1/previews/$PV" | jq_ "d['base_change'].get('head')")" = "$(api GET "/v1/previews/$MAINPV" | jq_ "d['head']")" ] || { echo "the change should be compared with main where it left it: $(api GET "/v1/previews/$PV" | jq_ "d.get('base_change')")"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{content_type}' "http://$API/v1/previews/$PV/screenshot?base=1")" = image/png ] || { echo "main's screenshot should be there to compare"; exit 1; }
# Where it differs, as a picture: against main's, and against the push before.
curl -sS -o "$HOME_DIR/diff.png" "http://$API/v1/previews/$PV/screenshot?diff=base"
DIFF=$(python3 -c "import sys; b=open(sys.argv[1],'rb').read(); print('PNG' if b[:4]==b'\x89PNG' else 'not a PNG: ' + b[:200].decode('utf-8','replace'))" "$HOME_DIR/diff.png")
[ "$DIFF" = PNG ] || { echo "the diff against main should be a PNG: $DIFF"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{content_type}' "http://$API/v1/previews/$PV/screenshot?diff=previous")" = image/png ] || { echo "the diff against the push before should be a PNG"; exit 1; }
# A public preview's pull request comment shows where it differs from main through a signed
# link of its own, which opens without credentials; the screenshot's signature doesn't open it.
api PUT "/v1/previews/projects/$PRJ/settings" '{"access":"public"}' >/dev/null
DIFF_URL=""
for _ in $(seq 1 60); do
  DIFF_URL=$(api GET "/v1/code/changes/$CHG" | jq_ "((d.get('preview') or {}).get('environment') or {}).get('diff_url') or ''")
  [ -n "$DIFF_URL" ] && break
  sleep 0.5
done
DIFF_PATH="/${DIFF_URL#*://*/}"
[ "$(curl -s -o /dev/null -w '%{content_type}' "http://$API$DIFF_PATH")" = image/png ] || { echo "the signed diff link should open the diff: $DIFF_URL"; exit 1; }
SHOT_NOW=$(api GET "/v1/code/changes/$CHG" | jq_ "d['preview']['environment']['screenshot_url']")
SWAPPED="/${SHOT_NOW#*://*/}"; SWAPPED="${SWAPPED/screenshot.png?/screenshot.png?diff=base&}"
[ "$(curl -s -o /dev/null -w '%{http_code}' "http://$API$SWAPPED")" = 403 ] || { echo "the screenshot's signature shouldn't open the diff"; exit 1; }
api PUT "/v1/previews/projects/$PRJ/settings" '{"access":"link"}' >/dev/null
# Main's load where it was compared comes along (to say "300 ms slower than main").
MAIN_LOAD=$(api GET "/v1/previews/$PV" | jq_ "d['base_change'].get('load_ms')")
case "$MAIN_LOAD" in [0-9]*) ;; *) echo "the comparison with main should carry main's load time: $MAIN_LOAD"; exit 1;; esac
# Main's page misses a language and a title too: none of that is the change's.
A11Y_NEW=$(api GET "/v1/previews/$PV" | jq_ "d['base_change'].get('a11y_added')")
[ "$A11Y_NEW" = 0 ] || { echo "the change adds no accessibility problems main doesn't have: $A11Y_NEW"; exit 1; }
# Its page logs "boom" and main's doesn't: that one is the change's.
ERRORS_NEW=$(api GET "/v1/previews/$PV" | jq_ "d['base_change'].get('errors_added')")
case "$ERRORS_NEW" in [0-9]*) ;; *) echo "the comparison with main should count the change's own errors: $ERRORS_NEW"; exit 1;; esac
# The other page, taken in the same session and compared with main's.
PAGES=$(api GET "/v1/previews/$PV" | jq_ "'; '.join('%s: %d error(s), %s%% against main' % (x['path'], x['errors'], x['base_percent']) for x in d.get('pages', []))")
case "$PAGES" in "/pricing: "[1-9]*" error(s), "*"% against main") ;; *) echo "/pricing should be taken, with its 404, and compared with main's: $PAGES"; exit 1;; esac
case "$PAGES" in *"wiki:/: 0 error(s), "*"% against main"*) ;; *) echo "the wiki service's page should be taken in its own run and compared: $PAGES"; exit 1;; esac
[ "$(curl -s -o /dev/null -w '%{content_type}' "http://$API/v1/previews/$PV/screenshot?page=%2Fpricing")" = image/png ] || { echo "/pricing's screenshot should be there"; exit 1; }
# Visual review: the change's page differs from main's, so it waits for someone; accepted, it's done.
api PUT "/v1/previews/projects/$PRJ/settings" '{"visual_review":"required"}' >/dev/null
REVIEW=$(api GET "/v1/previews/$PV" | jq_ "(d.get('visual_review') or {}).get('state')")
[ "$REVIEW" = changed ] || { echo "a page that differs from main's should wait for a review: $REVIEW"; exit 1; }
# Under the project's visual threshold (its pages differ by about 0.1%) nothing waits.
api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"visual_threshold":5}}' >/dev/null
UNDER=$(api GET "/v1/previews/$PV" | jq_ "(d.get('visual_review') or {}).get('state')")
[ "$UNDER" = unchanged ] || { echo "a change under the visual threshold shouldn't wait for a review: $UNDER"; exit 1; }
api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"visual_threshold":0}}' >/dev/null
# Required: the change can't land in roll meanwhile.
# (roll code hears of the rule with the preview's next report.)
for _ in $(seq 1 120); do
  BLOCKED=$(api GET "/v1/code/changes/$CHG" | jq_ "[b['kind'] for b in d.get('blockers', [])]")
  case "$BLOCKED" in *visual*) break;; esac
  sleep 0.5
done
case "$BLOCKED" in *visual*) ;; *) echo "a required visual review should keep the change from landing: $BLOCKED"; exit 1;; esac
# Accepting what wasn't seen (an older screenshot's head) is refused; what was, accepted.
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://$API/v1/previews/$PV/visual/accept" -H 'content-type: application/json' -d '{"head":"cs_not_this_one"}')" = 409 ] || { echo "accepting an older screenshot should be refused"; exit 1; }
SEEN=$(api GET "/v1/previews/$PV" | jq_ "d['visual_review']['head']")
# One page at a time (the first page and web:/ both differ from main's): one accepted, the change
# still waits for the other.
CHANGED_PAGES=$(api GET "/v1/previews/$PV" | jq_ "len(d['visual_review']['pages'])")
[ "$CHANGED_PAGES" -ge 2 ] || { echo "two pages should differ from main's to review them one at a time: $CHANGED_PAGES"; exit 1; }
ONE=$(api GET "/v1/previews/$PV" | jq_ "d['visual_review']['pages'][0]['path']")
PARTLY=$(api POST "/v1/previews/$PV/visual/accept" "$(python3 -c 'import json,sys; print(json.dumps({"head": sys.argv[1], "path": sys.argv[2]}))' "$SEEN" "$ONE")" | jq_ "'%s, %d of %d' % (d['visual_review']['state'], sum(1 for x in d['visual_review']['pages'] if x['by']), len(d['visual_review']['pages']))")
case "$PARTLY" in "changed, 1 of "*) ;; *) echo "accepting one page should leave the others waiting: $PARTLY"; exit 1;; esac
case "$(api GET "/v1/code/changes/$CHG" | jq_ "[b['kind'] for b in d.get('blockers', [])]")" in *visual*) ;; *) echo "a partly accepted required review should still block landing"; exit 1;; esac
REVIEW=$(api POST "/v1/previews/$PV/visual/accept" "{\"head\":\"$SEEN\"}" | jq_ "'%s by %s' % (d['visual_review']['state'], d['visual_review']['by'])")
[ "$REVIEW" = "accepted by operator" ] || { echo "accepting should settle the review: $REVIEW"; exit 1; }
case "$(api GET "/v1/code/changes/$CHG" | jq_ "[b['kind'] for b in d.get('blockers', [])]")" in *visual*) echo "an accepted review shouldn't block landing"; exit 1;; esac
# Turned off, it goes.
OFF=$(api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"baseline":false}}'); echo "$OFF" | grep -q "\"baseline\":false" || { echo "turning the baseline off: $OFF"; exit 1; }
for _ in $(seq 1 120); do
  [ "$(api GET "/v1/previews?project=$PRJ" | jq_ "len(d)")" = 1 ] && break
  sleep 0.5
done
[ "$(api GET "/v1/previews?project=$PRJ" | jq_ "len(d)")" = 1 ] || { echo "with github.baseline off, main's preview should go"; exit 1; }
PHONE=$(curl -sS "http://$API/v1/previews/$PV/screenshot?view=phone" | python3 -c "import sys, struct; b=sys.stdin.buffer.read(); print(f'{struct.unpack(\">I\", b[16:20])[0]} px wide, PNG' if b[:4]==b'\x89PNG' else 'not a PNG')")
case "$PHONE" in "780 px wide, PNG") ;; *) echo "the phone-width screenshot should be 390 px at twice the density: $PHONE"; exit 1;; esac

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
PARENTS_OK=$( (api GET "/v1/redis/databases/$REDIS" | jq_ "d['id']"; api GET /v1/storage/buckets/assets | jq_ "d['name']"; api GET "/v1/nosql/databases/$NOSQL" | jq_ "d['id']"; api GET "/v1/search/clusters/$SEARCH" | jq_ "d['id']") | wc -l)
SEARCH_BRANCH_GONE=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${ROLL_ADMIN_TOKEN:-}" "http://$API/v1/search/clusters/$BRANCH_SEARCH")
[ "$SEARCH_BRANCH_GONE" = 404 ] || { echo "the search branch should be gone after landing ($SEARCH_BRANCH_GONE)"; exit 1; }
BRANCHES_LEFT=$(api GET /v1/redis/databases | jq_ "len([x for x in d if x['name'].startswith('pv-')])")

cat <<EOF
| What | Measured |
| --- | --- |
| Change opened → preview ready ($(echo "$P" | jq_ "len(d['resources'])") branches made, the service deployed) | $((T1 - T0)) ms |
| Parent bucket bytes / the branch's own bytes after writing 1 MiB / objects the branch sees | $PARENT_OBJECTS / $BRANCH_OBJECTS / $BRANCH_SEES |
| NoSQL branch's mask on app.users.email / masks on the parent | $MASKED / $PARENT_MASKED |
| NoSQL: app.users.email in the branch / a filter on the real address there | $NOSQL_READ / $NOSQL_FILTER |
| NoSQL: a mask on app.users_by_email's key, set | $KEY_MASK |
| What each branch hands the preview's code | $DATA |
| Core Web Vitals of the first page (the screenshot's load) | $VITALS |
| Visual review under a 5% visual threshold | $UNDER |
| X-Robots-Tag on the service's page / on the 401 without the link | $ROBOTS / $ROBOTS_LOCKED |
| The release's time on the preview's data (at its head) / on the change | $RELEASE / $CHG_RELEASE ms |
| The preview's link as a QR code (PNG, no-store) | $QR |
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
| A preview's own variable (\`roll preview env\`) in the running service / \`DATABASE_URL\` as one | FEATURE_FLAG=$VAR_SEEN / refused |
| ignore_paths: docs/ | $KEPT |
| \`[skip preview]\` in the title, then without it | removed, then a new preview |
| A share link (1 hour): in through it / revoked | 200 / 401 |
| What went wrong while the page loaded (a push whose page logs an error) | $HEALTH; new against main: $ERRORS_NEW |
| The page's change since the push before (screenshots) | $VISUAL |
| The phone-width screenshot | $PHONE |
| The change's page against main's preview | $BASE (main loads in $MAIN_LOAD ms) |
| Another page (\`github.pages\`) | $PAGES |
| Visual review (required): the change's page against main's, one page accepted, then all | changed (blockers $BLOCKED), then $PARTLY, then $REVIEW |
| Accessibility of the test page (it has no lang and no title) | $A11Y; new against main: $A11Y_NEW |
| The first page's speed, timed with its screenshot | $SPEED |
| The push before, at its own URL | ${EARLIER_HOST%%.*}: its own page, on the same data |
| Search: products.name in the branch / in the parent / the branch's term dictionary | $ES_READ / $ES_PARENT / $ES_TERMS |
| After landing: parents present / preview Redis branches left | $PARENTS_OK of 4 / $BRANCHES_LEFT |
EOF
