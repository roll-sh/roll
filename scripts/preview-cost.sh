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
#   STOP_AT=against scripts/preview-cost.sh --keep      # main's preview and the change's running, compared
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
    for c in $(docker ps -aq --filter "label=sh.roll.code.home=$HOME_DIR") $(docker ps -aq --filter "label=roll.home=$HOME_DIR"); do docker rm -f "$c" >/dev/null 2>&1 || true; done
    # And the previews' own networks (egress roll) a delete just before the stop didn't reach.
    for n in $(docker network ls -q --filter "label=sh.roll.code.home=$HOME_DIR"); do docker network rm "$n" >/dev/null 2>&1 || true; done
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
  ROLL_SEARCH_HTTP=127.0.0.1:${P}200 ROLL_PREVIEWS_SMTP=127.0.0.1:${P}525 \
  ROLL_NOSQL_CQL=127.0.0.1:${P}042 ROLL_NOSQL_CQL_SHARD_AWARE=127.0.0.1:${P}043 ROLL_NOSQL_ALTERNATOR=127.0.0.1:${P}000 ROLL_NOSQL_REST=127.0.0.1:${P}100 \
  ROLL_CODE_PREVIEW_DOMAIN=$DOMAIN ROLL_PRODUCTS=${PREVIEW_PRODUCTS:-code,postgres,redis,storage,nosql,queues,streams,tasks,search} RUST_LOG=${RUST_LOG:-info}
unset ROLL_ADMIN_TOKEN ROLL_CODE_TOKEN
# The comparison with main's preview at a test's pace (every 5 s, not every minute).
export ROLL_PREVIEWS_COMPARE_EVERY=5
# A deployment key, as a real node has (what roll seals with it: roll secrets, a preview's own variables).
export ROLL_SECRET_KEY=${PREVIEW_E2E_SECRET_KEY:-$(python3 -c 'import secrets; print(secrets.token_hex(32))')}
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
TOML='[preview]\nimage = \"'"$IMAGE"'\"\nrun = \"env > env.txt && python -m http.server $PORT\"\nport = 8000\nhealth = \"/\"\nmemory = 128\ncpus = 0.25\npages = [\"/index.html\"]\n[[preview.states]]\nname = \"repo-tab\"\npage = \"/\"\nsteps = [{ press = \"Tab\" }]\n'
# The service's API, as an OpenAPI document (github.openapi): the change removes one operation and adds one.
OPENAPI_MAIN='{\"paths\":{\"/users\":{\"get\":{\"parameters\":[{\"name\":\"limit\",\"in\":\"query\",\"schema\":{\"type\":\"integer\"}}]}},\"/users/{id}\":{\"delete\":{}}}}'
OPENAPI_CHG='"openapi.json":{"content":"{\"paths\":{\"/users\":{\"get\":{\"parameters\":[{\"name\":\"limit\",\"in\":\"query\",\"required\":true,\"schema\":{\"type\":\"integer\"}}]}},\"/orders\":{\"post\":{}}}}"}'
api POST /v1/code/repos/acme/web/land "{\"message\":\"init\",\"files\":{\"roll.toml\":{\"content\":\"$TOML\"},\"index.html\":{\"content\":\"main\"},\"openapi.json\":{\"content\":\"$OPENAPI_MAIN\"}}}" >/dev/null
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
# An address inside an array of objects, and one under a literal dotted key: masks reach both.
curl -sf -XPUT "$SEARCH_URL_PARENT/products/_doc/3?refresh=true" -H 'content-type: application/json' -d '{"name":"other","owner":[{"email":"ada@corp.example"}]}' >/dev/null
# An index whose name has dots (logs.2024): its mask is split off at the whole name.
curl -sf -XPUT "$SEARCH_URL_PARENT/logs.2024/_doc/1?refresh=true" -H 'content-type: application/json' -d '{"user":{"email":"eve@corp.example"}}' >/dev/null
curl -sf -XPUT "$SEARCH_URL_PARENT/products/_doc/4?refresh=true" -H 'content-type: application/json' -d '{"name":"dotted","owner.email":"bob@corp.example","stock":3}' >/dev/null
# A connector on the parent (roll search's native syncs, #318): its branches inherit it, waiting.
curl -sf -XPUT "$SEARCH_URL_PARENT/_connector/catalog-sync" -H 'content-type: application/json' -d '{"index_name":"content-synced","is_native":true,"service_type":"s3"}' >/dev/null || { echo "a connector on the parent search cluster"; exit 1; }
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
# A project rule can name a key in roll secrets instead of holding its value (roll://ENV/KEY): the
# preview's environment reads it there (untrusted), and a plain value's `${` stays as it is.
REFD=$(curl -s -o /dev/null -w '%{http_code}' -X PUT "http://$API/v1/secrets/projects/$PRJ/environments/staging/secrets/PAYMENTS_TEST_KEY" -H 'content-type: application/json' -d '{"value":"sk_test_from_staging_42"}')
case "$REFD" in 200|201) ;; *) echo "setting staging's PAYMENTS_TEST_KEY: HTTP $REFD"; exit 1;; esac
api PUT "/v1/previews/projects/$PRJ/settings" "{\"env\":{\"SENDGRID_API_KEY\":\"SG.from-production\",\"SMTP_HOST\":\"smtp.sendgrid.net\",\"PAYMENTS_KEY\":\"roll://staging/PAYMENTS_TEST_KEY\",\"GREETING\":\"hello \${USER}\"},\"enabled\":true,\"sleep_after_s\":60,\"release\":{\"image\":\"$IMAGE\",\"run\":\"true\"},\"masking\":{\"nosql:$NOSQL\":{\"app.users.email\":\"mask_default()\"},\"postgres:$PG\":{\"public.users.email\":\"email\"},\"search:$SEARCH\":{\"products.name\":\"redact\",\"products.owner.email\":\"redact\",\"logs.2024.user.email\":\"redact\"}}}" >/dev/null
# A mask on a key column can't hide it (the engine refuses to read such a table): the settings
# name it instead of letting a preview's pages fail.
KEY_MASK=$(curl -s -o /tmp/roll-previews-keymask.json -w '%{http_code}' -X PUT "http://$API/v1/previews/projects/$PRJ/settings" -H 'content-type: application/json' \
  -d "{\"masking\":{\"nosql:$NOSQL\":{\"app.users.email\":\"mask_default()\",\"app.users_by_email.email\":\"mask_default()\"}}}")
[ "$KEY_MASK" = 400 ] && grep -q "app.users_by_email's primary key" /tmp/roll-previews-keymask.json || { echo "a NoSQL mask on a key column should be refused, not $KEY_MASK $(cat /tmp/roll-previews-keymask.json)"; exit 1; }
# Nor can a string mask go into a search field that isn't text (a number, a vector): named too.
NUM_MASK=$(curl -s -o /tmp/roll-previews-nummask.json -w '%{http_code}' -X PUT "http://$API/v1/previews/projects/$PRJ/settings" -H 'content-type: application/json' \
  -d "{\"masking\":{\"search:$SEARCH\":{\"products.name\":\"redact\",\"products.owner.email\":\"redact\",\"products.stock\":\"redact\"}}}")
[ "$NUM_MASK" = 400 ] && grep -q "products.stock is a long field" /tmp/roll-previews-nummask.json || { echo "a string mask on a number field should be refused, not $NUM_MASK $(cat /tmp/roll-previews-nummask.json)"; exit 1; }

# The preview's base (development) links to the Postgres parent's password under a name of its
# own: roll secrets resolves nothing of the parent's for it (the preview branched it), and the
# preview's service gets the branch's under that name.
LINKED=$(curl -s -o /dev/null -w '%{http_code}' -X PUT "http://$API/v1/secrets/projects/$PRJ/environments/development/secrets/STAGING_PGPASS" -H 'content-type: application/json' -d "{\"link\":{\"kind\":\"postgres\",\"ref\":\"$PG\",\"field\":\"password\",\"mode\":\"live\"}}")
case "$LINKED" in 200|201) ;; *) echo "linking development's STAGING_PGPASS to $PG's password: HTTP $LINKED"; exit 1;; esac
# Where a follower of previews' events (roll MCP's Events) starts: what comes from now on.
EV0=$(api GET "/v1/previews/events?latest=1" | jq_ "d['next']")
say "open a change"
T0=$(now_ms)
CHG=$(api POST /v1/code/repos/acme/web/changes '{"title":"New page","message":"New page","base":"main","files":{"index.html":{"content":"preview of change 1"},'"$OPENAPI_CHG"'}}' | jq_ "d['id']")
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
# How its head came up, step by step (its timeline): every step named, in order.
STEPS=$(echo "$P" | jq_ "(lambda t: ' · '.join('%s %d ms' % (s['step'], s['at'] - (t['steps'][i - 1]['at'] if i else t['started_at'])) for i, s in enumerate(t['steps']) if s['step'] != 'ready'))(d['timeline']) if d.get('timeline') else ''")
case "$STEPS" in "data "*" · setup "*" · deploy "*" · checks "*) ;; *) echo "the preview's timeline should name each step in order: '$STEPS'"; exit 1;; esac
say "its timeline: $STEPS"
# A follower hears it made, then up, in order.
EVENTS=$(api GET "/v1/previews/events?after=$EV0" | jq_ "' → '.join(e['kind'] for e in d['events'] if e['preview'] == '$PV')")
case "$EVENTS" in "created → ready"*) ;; *) echo "a follower of the events should hear the preview made, then up: '$EVENTS'"; exit 1;; esac

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
curl -sS -D "$HOME_DIR/qr.h" -o "$HOME_DIR/qr.png" "http://$API/v1/previews/$PV/qr.png"
QR=$(head -c 8 "$HOME_DIR/qr.png" | od -An -c | tr -d ' \n')
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
# Its request log (Vercel's): the visits above through roll, newest first, never with a query
# (the first carried the share token).
RQ=$(curl -sS "http://$API/v1/previews/$PV/requests")
REQS=$(echo "$RQ" | python3 -c "import json,sys; r=json.load(sys.stdin)['requests']; print('%d; newest: %s; queries: %s' % (len(r), ' '.join(str(r[0][k]) for k in ('method', 'path', 'status')) if r else 'none', any('?' in x['path'] for x in r)))")
case "$REQS" in *"newest: GET / 200; queries: False") ;; *) echo "the preview's request log should list its visits without their queries: $REQS"; exit 1;; esac
# What its running service uses (`…/metrics`): some memory, against what it runs with.
METRICS=$(curl -sS "http://$API/v1/previews/$PV/metrics" | python3 -c "import json,sys; m=json.load(sys.stdin)['services']; print('; '.join('%s %.2f CPU %d of %d MB' % (x['service'], x['cpu'], x['memory_bytes'] >> 20, x['memory_mb']) for x in m) if m and all(x['memory_bytes'] > 0 for x in m) else 'none')")
[ "$METRICS" != none ] || { echo "a running preview's services should report what they use"; exit 1; }
# And from the CLI (`roll preview requests`): its visits, and none with --errors.
# (Read whole first: `grep -q` stops at its match, and with pipefail the CLI's next write to the
# closed pipe would fail the step once there are enough requests.)
REQS=$(ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview requests "$PV")
echo "$REQS" | grep -q " 200 GET    /  " || { echo "roll preview requests should list GET / 200: $(echo "$REQS" | head -3)"; exit 1; }
[ -z "$(ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview requests "$PV" --errors)" ] || { echo "roll preview requests --errors should list only 4xx and 5xx"; exit 1; }
# The service's DATABASE_URL is the branch's own role, which can't read the WAL (where the pages
# from before the mask are) and isn't a superuser.
APP_DB=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt" | sed -n 's/^DATABASE_URL=//p' | sed 's#host.docker.internal#127.0.0.1#')
# The names a roll secrets link to the parent would use are the branch's too (PGUSER, PGPASSWORD
# from its app URI), so an inherited link can't hand out the parent's under them.
APP_ENV=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt")
PG_NAMES=$(python3 -c 'import sys,urllib.parse as u; e=dict(l.split("=",1) for l in sys.stdin.read().splitlines() if "=" in l); p=u.urlsplit(e.get("DATABASE_URL","")); print("ok" if e.get("PGUSER")==u.unquote(p.username or "") and e.get("PGPASSWORD")==u.unquote(p.password or "") and e.get("PGPASSWORD") else "PGUSER=%s, PGPASSWORD set: %s" % (e.get("PGUSER"), bool(e.get("PGPASSWORD"))))' <<<"$APP_ENV")
[ "$PG_NAMES" = ok ] || { echo "the service's PGUSER/PGPASSWORD should be the branch's: $PG_NAMES"; exit 1; }
FOLLOWED=$(python3 -c 'import sys,urllib.parse as u; e=dict(l.split("=",1) for l in sys.stdin.read().splitlines() if "=" in l); p=u.urlsplit(e.get("DATABASE_URL","")); s=e.get("STAGING_PGPASS"); print("ok" if s and s==u.unquote(p.password or "") else "STAGING_PGPASS is %s" % ("unset" if s is None else "not the branch'"'"'s password"))' <<<"$APP_ENV")
[ "$FOLLOWED" = ok ] || { echo "a secrets link to the branched parent should give the branch's password: $FOLLOWED"; exit 1; }
echo "$APP_ENV" | grep -qx 'PAYMENTS_KEY=sk_test_from_staging_42' || { echo "roll://staging/PAYMENTS_TEST_KEY should be staging's value: $(echo "$APP_ENV" | grep '^PAYMENTS_KEY=' | cut -c1-40)"; exit 1; }
echo "$APP_ENV" | grep -qxF 'GREETING=hello ${USER}' || { echo "a plain value's \${ should stay as it is: $(echo "$APP_ENV" | grep '^GREETING=')"; exit 1; }
# Its bucket's key reaches only its branch: production's bucket (the parent) refuses it, as every
# credential a preview's code holds is the branch's own (search's data-only key below too).
PV_AK=$(echo "$APP_ENV" | sed -n 's/^AWS_ACCESS_KEY_ID=//p'); PV_SK=$(echo "$APP_ENV" | sed -n 's/^AWS_SECRET_ACCESS_KEY=//p')
[ -n "$PV_AK" ] && [ "$PV_AK" != "$AWS_ACCESS_KEY_ID" ] || { echo "the preview's service should hold a bucket key of its own, not the operator's"; exit 1; }
OWN_LS=$(AWS_ACCESS_KEY_ID=$PV_AK AWS_SECRET_ACCESS_KEY=$PV_SK $S3 ls "s3://$BRANCH_BUCKET/" >/dev/null 2>&1 && echo ok || echo refused)
# (Captured first: with pipefail, `aws … | grep -q` fails on aws's own exit code.)
PARENT_SAID=$(AWS_ACCESS_KEY_ID=$PV_AK AWS_SECRET_ACCESS_KEY=$PV_SK $S3 ls "s3://assets/" 2>&1 || true)
case "$PARENT_SAID" in *AccessDenied*) PARENT_LS=refused ;; *) PARENT_LS="not refused" ;; esac
[ "$OWN_LS/$PARENT_LS" = "ok/refused" ] || { echo "the preview's bucket key should reach only its branch: own $OWN_LS, parent $PARENT_LS"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "http://$API/v1/previews/projects/$PRJ/settings" -H 'content-type: application/json' -d '{"env":{"X":"roll://other@staging/KEY"}}')" = 400 ] || { echo "a reference into another project should be refused"; exit 1; }
# A preview's own variable may be a reference too (its setter's reveal: the operator's here); a
# look-alike is refused, never kept as text.
[ "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "http://$API/v1/previews/$PV/env" -H 'content-type: application/json' -d '{"vars":{"X":" roll://staging/PAYMENTS_TEST_KEY"}}')" = 400 ] || { echo "a look-alike reference should be refused"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "http://$API/v1/previews/$PV/env" -H 'content-type: application/json' -d '{"vars":{"X":"roll://staging/PAYMENTS_TEST_KEY"}}')" = 200 ] || { echo "a preview's own variable should take a reference its setter may reveal"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "http://$API/v1/previews/$PV/env" -H 'content-type: application/json' -d '{"vars":{"X":null}}')" = 200 ] || { echo "removing it should work"; exit 1; }
case "$APP_DB" in postgres://br_*) ;; *) echo "the service's DATABASE_URL should be the branch's own role, not ${APP_DB%%@*}…"; exit 1;; esac
# Its mail goes to roll's catcher, signed in as the preview, never to the provider production
# uses: the key is gone, and what it sends is caught and listed (sent here as its service would).
ENVTXT=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt")
echo "$ENVTXT" | grep -q '^SENDGRID_API_KEY=' && { echo "a preview's service shouldn't get the mail provider's key"; exit 1; }
SMTP_USER=$(echo "$ENVTXT" | sed -n 's/^SMTP_USER=//p'); SMTP_PASSWORD=$(echo "$ENVTXT" | sed -n 's/^SMTP_PASSWORD=//p'); SMTP_PORT=$(echo "$ENVTXT" | sed -n 's/^SMTP_PORT=//p')
[ "$SMTP_USER" = "$PV" ] && [ "$(echo "$ENVTXT" | sed -n 's/^SMTP_HOST=//p')" = host.docker.internal ] || { echo "the service's SMTP should be roll's catcher, as the preview: $SMTP_USER"; exit 1; }
python3 - "$SMTP_PORT" "$SMTP_USER" "$SMTP_PASSWORD" <<'PY'
import smtplib, sys
port, user, password = int(sys.argv[1]), sys.argv[2], sys.argv[3]
with smtplib.SMTP("127.0.0.1", port, timeout=10) as s:
    s.login(user, password)
    s.sendmail("shop@example.com", ["ada@corp.example"], "Subject: Your order\r\n\r\nThanks, Ada.\r\n")
try:
    with smtplib.SMTP("127.0.0.1", port, timeout=10) as s:
        s.login(user, "guess")
    sys.exit("a wrong password signed in")
except smtplib.SMTPAuthenticationError:
    pass
PY
MAIL=$(api GET "/v1/previews/$PV/mail" | jq_ "'%s to %s' % (d['mail'][0]['subject'], ','.join(d['mail'][0]['to'])) if d.get('mail') else ''")
[ "$MAIL" = "Your order to ada@corp.example" ] || { echo "the preview's mail should be caught and listed: $MAIL"; exit 1; }
ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview mail "$PV" | grep -q "to ada@corp.example  Your order" || { echo "roll preview mail should list it"; exit 1; }
# A reviewer's connection from a laptop (`roll preview connect`): the same role, never the parent's.
CONNECT=$(ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview connect "$PV" --kind postgres | sed -n 's/^DATABASE_URL=//p')
case "$CONNECT" in postgres://br_*) ;; *) echo "roll preview connect should hand out the branch's own role, not ${CONNECT%%@*}…"; exit 1;; esac
role() { echo "$1" | sed 's#^postgres://\([^:@]*\)[:@].*#\1#'; }
[ "$(role "$CONNECT")" = "$(role "$APP_DB")" ] || { echo "the connection's role should be the service's: $(role "$CONNECT") / $(role "$APP_DB")"; exit 1; }
# The service's ELASTICSEARCH_URL is the branch's: it reads the parent's document; a write stays in
# the branch.
# Only the branch's data-only credential (#607): the URL carries no login, the key goes beside it.
APP_ENV=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt")
APP_ES=$(echo "$APP_ENV" | sed -n 's/^ELASTICSEARCH_URL=//p' | sed 's#host.docker.internal#127.0.0.1#')
APP_KEY=$(echo "$APP_ENV" | sed -n 's/^ELASTICSEARCH_API_KEY=//p')
[ -n "$APP_ES" ] && [ -n "$APP_KEY" ] || { echo "the service should have ELASTICSEARCH_URL and ELASTICSEARCH_API_KEY"; exit 1; }
case "$APP_ES" in *://*@*) echo "the service's ELASTICSEARCH_URL should carry no login (the branch's own login stays roll's)"; exit 1;; esac
es() { curl -sf -H "Authorization: ApiKey $APP_KEY" "$@"; }
# products.name is masked in the branch (the project's masking), and only there.
ES_READ=$(es "$APP_ES/products/_doc/1" | jq_ "d['_source']['name']")
[ "$ES_READ" = "[masked]" ] || { echo "the search branch should read the parent's document with products.name masked, not $ES_READ"; exit 1; }
ES_PARENT=$(curl -sf "$SEARCH_URL_PARENT/products/_doc/1" | jq_ "d['_source']['name']")
[ "$ES_PARENT" = "parent" ] || { echo "masking the branch shouldn't touch the parent's document: $ES_PARENT"; exit 1; }
ES_DEEP=$( (es "$APP_ES/products/_doc/3" | jq_ "d['_source']['owner'][0]['email']"; es "$APP_ES/products/_doc/4" | jq_ "d['_source']['owner.email']") | paste -sd/)
[ "$ES_DEEP" = "[masked]/[masked]" ] || { echo "a mask should reach a field in an array of objects and under a dotted key: $ES_DEEP"; exit 1; }
[ "$(curl -sf "$SEARCH_URL_PARENT/products/_doc/3" | jq_ "d['_source']['owner'][0]['email']")" = "ada@corp.example" ] || { echo "the parent's address should stay (the check's premise)"; exit 1; }
ES_DOTTED=$(es "$APP_ES/logs.2024/_doc/1" | jq_ "d['_source']['user']['email']")
[ "$ES_DOTTED" = "[masked]" ] || { echo "a mask on an index whose name has dots (logs.2024) should reach it: $ES_DOTTED"; exit 1; }
[ "$(curl -sf "$SEARCH_URL_PARENT/logs.2024/_doc/1" | jq_ "d['_source']['user']['email']")" = "eve@corp.example" ] || { echo "the parent's logs.2024 should keep its address"; exit 1; }
# Nor can the preview's code read the masked value back out of the segments the branch shares
# with its parent (a term dictionary keeps a replaced document's terms): the parent's has it,
# the branch's doesn't.
TERMS() { curl -sf -XPOST "${@:2}" "$1/products/_terms_enum" -H 'content-type: application/json' -d '{"field":"name.keyword","string":"par"}' | jq_ "d.get('terms')"; }
[ "$(TERMS "$SEARCH_URL_PARENT")" = "['parent']" ] || { echo "the parent's term dictionary should have the value (the check's premise): $(TERMS "$SEARCH_URL_PARENT")"; exit 1; }
ES_TERMS=$(TERMS "$APP_ES" -H "Authorization: ApiKey $APP_KEY")
[ "$ES_TERMS" = "[]" ] || { echo "the masked value is still in the branch's term dictionary: $ES_TERMS"; exit 1; }
# The parent's connector is inherited waiting for adoption (roll search's #318): a sync asked of the
# branch never runs (it would read production's source into the preview, past its masks), while
# the parent's does (the premise), and the branch stays masked.
console() { api POST "/v1/search/clusters/$1/request" "{\"method\":\"$2\",\"path\":\"$3\"${4:+,\"body\":$4}}"; }
answer() { jq_ "__import__('json').loads(d.get('body') or '{}').get('$1', '')"; }
PJOB=$(console "$SEARCH" POST /_connector/_sync_job '{"id":"catalog-sync"}' | answer id)
BJOB=$(console "$BRANCH_SEARCH" POST /_connector/_sync_job '{"id":"catalog-sync"}' | answer id)
[ -n "$PJOB" ] && [ -n "$BJOB" ] || { echo "a sync job of the connector on the parent and on the branch: '$PJOB' / '$BJOB'"; exit 1; }
for _ in $(seq 1 120); do
  PSTATE=$(console "$SEARCH" GET "/_connector/_sync_job/$PJOB" | answer status)
  [ "$PSTATE" != pending ] && break
  sleep 1
done
BSTATE=$(console "$BRANCH_SEARCH" GET "/_connector/_sync_job/$BJOB" | answer status)
case "$PSTATE" in in_progress|error|completed) [ "$BSTATE" = pending ] ;; *) false ;; esac || { echo "the parent's sync should run (it did: $PSTATE) and the branch's wait for adoption (pending, not $BSTATE)"; exit 1; }
[ "$(es "$APP_ES/products/_doc/1" | jq_ "d['_source']['name']")" = "[masked]" ] || { echo "the search branch should stay masked beside its inherited connector"; exit 1; }
# A field only the branch's documents have (price): its mapping gains it, and the next head's
# schema changes say so.
es -XPUT "$APP_ES/products/_doc/2?refresh=true" -H 'content-type: application/json' -d '{"name":"branch","price":9.5}' >/dev/null
ES_PARENT_SEES=$(curl -s -o /dev/null -w '%{http_code}' "$SEARCH_URL_PARENT/products/_doc/2")
[ "$ES_PARENT_SEES" = 404 ] || { echo "a write to the search branch reached the parent ($ES_PARENT_SEES)"; exit 1; }
# The branch's data-only key, as production's link gets: it reads and writes documents (above),
# and is refused making keys.
KEY_READ=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: ApiKey $APP_KEY" "$APP_ES/products/_doc/1")
KEY_MINT=$(curl -s -o /dev/null -w '%{http_code}' -XPOST -H "Authorization: ApiKey $APP_KEY" -H 'content-type: application/json' "$APP_ES/_security/api_key" -d '{"name":"more"}')
[ -n "$APP_KEY" ] && [ "$KEY_READ" = 200 ] && [ "$KEY_MINT" = 403 ] || { echo "the search branch's ELASTICSEARCH_API_KEY should read (200, got $KEY_READ) and be refused making keys (403, got $KEY_MINT); key set: $([ -n "$APP_KEY" ] && echo yes || echo no)"; exit 1; }
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
# Python's server writes its request lines on stderr: the followed log says so (`event: stderr`,
# before the event's `data:` line, so a client acting on each line knows its stream).
grep -B1 "pushed-log-line" "$FOLLOW" | grep -q "^event: stderr" || { echo "the request's line came on stderr and should say so:"; cat "$FOLLOW"; exit 1; }
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
# Resolved (what the page's Resolve opens roll to do), it folds away there.
NOTE_ID=$(echo "$NOTES" | jq_ "d[0]['id']")
[ "$(api GET "/v1/previews/$PV/notes" | jq_ "[n['id'] for n in d['notes']]")" = "['$NOTE_ID']" ] || { echo "the preview's open notes should list the page's, with its id"; exit 1; }
ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview notes "$PV" | grep -q "^$NOTE_ID  / body > h1  by .*: The heading is cut off" || { echo "roll preview notes should list it: $(ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview notes "$PV")"; exit 1; }
# A reply to it (the sheet's Reply: a comment review naming the note) is listed under it on the page.
api POST "/v1/code/changes/$CHG/reviews" "{\"verdict\":\"comment\",\"body\":\"**Reply on the preview** · \`$NOTE_ID\`\\n\\nWhich width?\"}" >/dev/null
REPLIES=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/__roll/feedback.json?path=%2F" | jq_ "[r['note'] for n in d for r in n.get('replies', [])]")
[ "$REPLIES" = "['Which width?']" ] || { echo "the note's reply should be listed under it on the page: $REPLIES"; exit 1; }
[ "$(api POST "/v1/previews/$PV/notes/$NOTE_ID/resolve" '{}' | jq_ "d.get('resolved')")" = "$NOTE_ID" ] || { echo "resolving the page's note should answer with it"; exit 1; }
RESOLVED=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/__roll/feedback.json?path=%2F" | jq_ "len(d)")
[ "$RESOLVED" = 0 ] && [ "$(api GET "/v1/previews/$PV/notes" | jq_ "len(d['notes'])")" = 0 ] || { echo "a resolved note should fold away on the page and from the open notes: $RESOLVED left"; exit 1; }
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

say "a shell in the preview's service (roll preview shell): what's typed runs in its container, with its variables"
# Keys from a pipe (no terminal, so no raw mode), the screen on stdout until the shell's `exit`.
SCREEN=$(printf 'echo "seen $ROLL_PREVIEW"; exit\r' | ROLL_API_URL="http://$API" timeout 60 "$ROOT/target/debug/roll" preview shell "$PV" --service web 2>&1) || { echo "roll preview shell failed: $SCREEN"; exit 1; }
case "$SCREEN" in *"seen $PV"*) ;; *) echo "the shell should run in the service's container with its variables: $SCREEN"; exit 1;; esac
api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d]" | grep -q "opened a shell in web" || { echo "the activity should say a shell was opened"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://$API/v1/previews/$PV/shell/sessions" -H 'content-type: application/json' -d '{"service":"nope"}')" = 404 ] || { echo "a shell in a service the preview hasn't should be 404"; exit 1; }

say "files in and out of the preview's service (roll preview cp)"
echo "fixture for $PV" > "$HOME_DIR/fixture.txt"
rm -f "$HOME_DIR/back.txt"
ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview cp "$PV" "$HOME_DIR/fixture.txt" :/tmp/ --service web >/dev/null || { echo "roll preview cp should put a file into the service"; exit 1; }
ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview cp "$PV" :/tmp/fixture.txt "$HOME_DIR/back.txt" --service web >/dev/null || { echo "roll preview cp should bring the file back"; exit 1; }
[ "$(cat "$HOME_DIR/back.txt" 2>/dev/null)" = "fixture for $PV" ] || { echo "the file brought back should be the one put in: $(cat "$HOME_DIR/back.txt" 2>/dev/null)"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' "http://$API/v1/previews/$PV/files?service=web&path=/nothing/here")" = 404 ] || { echo "nothing at a path should be 404"; exit 1; }
api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d]" | grep -q "copied files into web at /tmp" || { echo "the activity should say files were copied in"; exit 1; }

say "a port of the preview's service on this machine (roll preview forward), from inside it"
# (`P` is the preview's JSON by now: the port block from the environment.)
FWD_PORT=${PREVIEW_E2E_PORTS:-27}977
ROLL_API_URL="http://$API" "$ROOT/target/debug/roll" preview forward "$PV" 8000 --service web --local-port "$FWD_PORT" 2>/dev/null &
FWD_PID=$!
for _ in $(seq 1 50); do ss -ltn | grep -q ":$FWD_PORT " && break; sleep 0.2; done
FWDED=$(curl -s --max-time 30 "http://127.0.0.1:$FWD_PORT/env.txt" | grep -c "^ROLL_PREVIEW=$PV$" || true)
kill "$FWD_PID" 2>/dev/null || true; wait "$FWD_PID" 2>/dev/null || true
[ "$FWDED" = 1 ] || { echo "the forwarded port should reach the service from inside it: $FWDED"; exit 1; }
api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d]" | grep -q "forwarded port 8000 of web" || { echo "the activity should say the port was forwarded"; exit 1; }
for _ in $(seq 1 30); do [ "$(docker ps -aq --filter name=roll-previews-fwd- | wc -l)" = 0 ] && break; sleep 0.5; done
[ "$(docker ps -aq --filter name=roll-previews-fwd- | wc -l)" = 0 ] || { echo "a forward's relay should go with its connection"; exit 1; }

say "traces: with the project's traces on, the service gets OpenTelemetry's variables, spans it posts are the preview's traces, and an error naming one opens it"
api PUT "/v1/previews/projects/$PRJ/settings" '{"traces":true}' >/dev/null
# It restarts with them (its deploy's fingerprint): the env it writes at start says so.
OTEL=""
for _ in $(seq 1 120); do
  OTEL=$(curl -sS -b "$JAR" -H "Host: $HOST" "http://$API/env.txt" | sed -n 's/^OTEL_EXPORTER_OTLP_ENDPOINT=//p')
  [ -n "$OTEL" ] && break
  sleep 1
done
[ -n "$OTEL" ] || { echo "the service should get OTEL_EXPORTER_OTLP_ENDPOINT with traces on"; exit 1; }
# The span an SDK in it would send, to the endpoint and token it was given. Posted from this host:
# on ai-server the firewall keeps containers from the host (as with the mail catcher and host
# Postgres), so the container's own `host.docker.internal` hop can't be checked here.
OTEL_HERE=$(echo "$OTEL" | sed 's#host.docker.internal#127.0.0.1#')
NOW_NS=$(date +%s%N)
SPANS="{\"resourceSpans\":[{\"resource\":{\"attributes\":[{\"key\":\"service.name\",\"value\":{\"stringValue\":\"web\"}}]},\"scopeSpans\":[{\"spans\":[{\"traceId\":\"abababababababababababababababab\",\"spanId\":\"0101010101010101\",\"name\":\"GET /checkout\",\"startTimeUnixNano\":\"$((NOW_NS - 40000000))\",\"endTimeUnixNano\":\"$NOW_NS\"},{\"traceId\":\"abababababababababababababababab\",\"spanId\":\"0202020202020202\",\"parentSpanId\":\"0101010101010101\",\"name\":\"charge card\",\"startTimeUnixNano\":\"$((NOW_NS - 30000000))\",\"endTimeUnixNano\":\"$((NOW_NS - 5000000))\",\"status\":{\"code\":2}}]}]}]}"
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' -d "$SPANS" "$OTEL_HERE/v1/traces")" = 200 ] || { echo "the service's OpenTelemetry endpoint should take its spans: $OTEL_HERE"; exit 1; }
TRACES=$(api GET "/v1/previews/$PV/traces" | jq_ "[(t['name'], t['spans'], t['error']) for t in d['traces']]")
case "$TRACES" in *"('GET /checkout', 2, True)"*) ;; *) echo "the span posted should be the preview's trace: $TRACES"; exit 1;; esac
ROUTES=$(api GET "/v1/previews/$PV/routes" | jq_ "[(r['name'], r['count']) for r in d['routes']]")
case "$ROUTES" in *"('GET /checkout', 1)"*) ;; *) echo "the trace should be its route's: $ROUTES"; exit 1;; esac
# A request that runs one query a row (an N+1): its route says the statement, literals taken out.
N1=$(NOW_NS="$NOW_NS" python3 -c '
import json, os
now = int(os.environ["NOW_NS"]); t = "cd" * 16
spans = [{"traceId": t, "spanId": "%016x" % 1, "name": "GET /orders", "startTimeUnixNano": str(now - 50000000), "endTimeUnixNano": str(now)}]
spans += [{"traceId": t, "spanId": "%016x" % (i + 2), "parentSpanId": "%016x" % 1, "name": "SELECT", "startTimeUnixNano": str(now - 40000000 + i * 1000000), "endTimeUnixNano": str(now - 39500000 + i * 1000000),
  "attributes": [{"key": "db.statement", "value": {"stringValue": "SELECT * FROM items WHERE order_id = %d" % i}}]} for i in range(12)]
print(json.dumps({"resourceSpans": [{"resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "web"}}]}, "scopeSpans": [{"spans": spans}]}]}))')
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' -d "$N1" "$OTEL_HERE/v1/traces")" = 200 ] || { echo "the N+1 trace should be taken"; exit 1; }
REPEATED=$(api GET "/v1/previews/$PV/routes" | jq_ "[(r['repeated'] or {}).get('statement', '') + ' ' + str((r['repeated'] or {}).get('count', 0)) for r in d['routes'] if r['name'] == 'GET /orders']")
[ "$REPEATED" = "['SELECT * FROM items WHERE order_id = ? 12']" ] || { echo "the route should say its repeated query: $REPEATED"; exit 1; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' -d '{}' "http://$API/v1/previews/$PV/otlp/not-the-token/v1/traces")" = 404 ] || { echo "another token should be 404"; exit 1; }
# An error its service logs with the request's trace id (as an SDK's logger does) opens that trace.
docker exec "roll-previews-$(echo "$HOST" | cut -d. -f1)" sh -c 'echo "ERROR card declined trace_id=abababababababababababababababab" > /proc/1/fd/2'
ERR_TRACE=""
for _ in $(seq 1 30); do
  ERR_TRACE=$(api GET "/v1/previews/$PV/errors" | jq_ "' '.join(g.get('trace') or '-' for g in d['groups'] if 'card declined' in g['title'])")
  [ "$ERR_TRACE" = abababababababababababababababab ] && break
  sleep 2
done
[ "$ERR_TRACE" = abababababababababababababababab ] || { echo "the error naming the trace should carry it: '$ERR_TRACE'"; exit 1; }

say "a screenshot of the service (github.screenshot), once per head"
# And /pricing, which the test service doesn't have (a 404 page, one failed request).
api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"screenshot":true,"pages":["/pricing","web:/","wiki:/"],"widths":[768],"hide":[".clock"],"budget":{"load_ms":60000},"openapi":"/openapi.json","endpoints":["/openapi.json"],"states":[{"name":"tab","page":"/","steps":[{"press":"Tab"}]}],"browsers":["firefox","webkit-phone"]}}' >/dev/null
# roll keeps a preview of the default branch (github.baseline): the change's screenshots are
# compared with its.
MAINPV=""
for _ in $(seq 1 120); do
  MAINPV=$(api GET "/v1/previews?project=$PRJ" | jq_ "next((p['id'] for p in d if p.get('branch') == 'main' and p['created_by'] == 'roll'), '')")
  [ -n "$MAINPV" ] && break
  sleep 0.5
done
[ -n "$MAINPV" ] || { echo "roll should keep a preview of main once screenshots are on (github.baseline)"; exit 1; }
# (Screenshots take one slot a node: main's, then this one, each with its pages, states and
# other browsers.)
for _ in $(seq 1 360); do
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
# Leaving link access retired the link handed out: its cookie opens nothing now, and the
# preview's new link lets its visitor in again.
[ "$(curl -s -o /dev/null -w '%{http_code}' -b "$JAR" -H "Host: $HOST" "http://$API/")" = 401 ] || { echo "a link retired by leaving link access should stop opening the preview"; exit 1; }
QUERY=$(api GET "/v1/previews/$PV" | jq_ "[x for x in d['services'] if x['name'] == 'web'][0]['share_url'].split('/?')[1]")
curl -sS -o /dev/null -L -c "$JAR" -b "$JAR" -H "Host: $HOST" "http://$API/?$QUERY" --max-time 120

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
  api POST "/v1/endpoints/$PG_EP/query" '{"sql":"CREATE TABLE preview_only (id int PRIMARY KEY, note text NOT NULL DEFAULT '"'"'x'"'"'); ALTER TABLE users ALTER COLUMN email SET NOT NULL"}' >/dev/null
  # Another change's migration lands on production first: a table the preview's branch doesn't
  # have, which isn't this change dropping it (neither a change nor a risk).
  api POST "/v1/endpoints/$PG_PARENT_EP/query" '{"sql":"CREATE TABLE landed_first (id int PRIMARY KEY)"}' >/dev/null
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
  # A note left open across the push: the next head's screenshot run crops its element.
  api POST "/v1/code/changes/$CHG/reviews" "{\"verdict\":\"comment\",\"body\":\"**Feedback on the preview** · [\`/\`](http://x/) · \`body\`\\n\\nThe whole page\"}" >/dev/null
  api POST "/v1/code/changes/$CHG/revise" '{"base":"main","message":"New page, again","files":{"index.html":{"content":"preview of change 1, again"},'"$OPENAPI_CHG"'}}' >/dev/null
  SCHEMA=""
  for _ in $(seq 1 120); do
    SCHEMA=$(api GET "/v1/previews/$PV" | jq_ "'\\n'.join(d.get('schema_changes') or [])")
    case "$SCHEMA" in *preview_only*) break;; esac
    sleep 0.5
  done
  case "$SCHEMA" in *"+ table public.preview_only"*) ;; *) echo "the schema changes should name the new table: $SCHEMA"; exit 1;; esac
  case "$SCHEMA" in *"+ field products.price "*) ;; *) echo "the schema changes should name the search branch's new field: $SCHEMA"; exit 1;; esac
  case "$SCHEMA" in *landed_first*) echo "a table production made after the branch isn't this change's: $SCHEMA"; exit 1;; esac
  # A column made NOT NULL is named as a migration risk; the new table isn't one.
  RISKS=$(api GET "/v1/previews/$PV" | jq_ "' | '.join(d.get('schema_risks') or [])")
  case "$RISKS" in "makes public.users.email (postgres:$PG) NOT NULL: "*) ;; *) echo "the schema's risks should name the NOT NULL column (and its database), and only it: $RISKS"; exit 1;; esac
  case "$RISKS" in *" | "*) echo "only the NOT NULL column is a risk: $RISKS"; exit 1;; esac
  ESCALATED=$(api POST "/v1/endpoints/$PG_EP/query" "{\"sql\":\"SELECT rolsuper FROM pg_roles WHERE rolname = '$APP_ROLE'\"}" | jq_ "d['results'][0]['rows'][0][0]")
  [ "$ESCALATED" = f ] || { echo "the branch's role became a superuser ($ESCALATED)"; exit 1; }
  say "$SCHEMA"
fi

say "a preview's own variable reaches its running service; roll's names are refused"
ROLL_CLI=${ROLL_CLI:-"$ROOT/target/debug/roll"}
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
# Kept sealed: the previews table holds no name or value of it.
SEALED=$(python3 -c 'import sqlite3, sys; r = sqlite3.connect(sys.argv[1]).execute("SELECT vars FROM previews WHERE id = ?", (sys.argv[2],)).fetchone(); v = (r or [""])[0] or ""; print("ok" if v.startswith("v1:") and "FEATURE_FLAG" not in v else "plain: " + v[:20])' "$HOME_DIR/previews.db" "$PV")
[ "$SEALED" = ok ] || { echo "a preview's own variables should be sealed at rest: $SEALED"; exit 1; }

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
api POST "/v1/code/changes/$CHG/revise" '{"base":"main","message":"Docs","files":{"index.html":{"content":"preview of change 1, again"},"docs/setup.md":{"content":"how to run it"},'"$OPENAPI_CHG"'}}' >/dev/null
DOCS_HEAD=$(api GET "/v1/code/changes/$CHG" | jq_ "d['head']")
until_head "$DOCS_HEAD"
# (What it runs is set by the converge just after it reports the new head: waited for.)
RUNS=""
# (Up to 2 minutes: at a load of 80 one converge has taken over one.)
for _ in $(seq 1 240); do
  RUNS=$(api GET "/v1/previews/$PV" | jq_ "d.get('runs') or ''")
  [ -n "$RUNS" ] && break
  sleep 0.5
done
[ "$(bare "$RUNS")" = "$(bare "$BUILD")" ] || { echo "a docs-only push should keep the services on $BUILD, not $RUNS"; exit 1; }
WAITED2=$(ROLL_API_URL="http://$API" "$ROLL_CLI" preview wait --change "$CHG" --timeout 30) || { echo "roll preview wait should pass for the docs-only head"; exit 1; }
# This one's page logs an error, which its screenshot's load must catch.
THIRD='preview of change 1, third<script>console.error(\"boom\")</script> <nav><a href=\"/gone\">a page that went</a></nav>'
api POST "/v1/code/changes/$CHG/revise" "{\"base\":\"main\",\"message\":\"Code\",\"files\":{\"index.html\":{\"content\":\"$THIRD\"},\"docs/setup.md\":{\"content\":\"how to run it\"},$OPENAPI_CHG}}" >/dev/null
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
[ "$PAGE" = 'preview of change 1, third<script>console.error("boom")</script> <nav><a href="/gone">a page that went</a></nav>' ] || { echo "a push that touches code should redeploy: the page says $PAGE"; exit 1; }
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
# It runs on its own copy of the data (taken before the next head's data step): a table the
# preview's branch gets now isn't in the earlier push's.
db_of() { curl -sS -b "$1" -H "Host: $2" "http://$API/env.txt" | sed -n 's/^DATABASE_URL=//p' | sed 's#host.docker.internal#127.0.0.1#'; }
psql_q() { docker run --rm --name roll-previews-psqltest --network host postgres:18-alpine psql "$1" -Atc "$2" 2>&1 | tail -1 || true; }
LIVE_DB=$(db_of "$JAR" "$HOST")
EARLIER_DB=$(db_of "$EJAR" "$EARLIER_HOST")
[ -n "$LIVE_DB" ] && [ -n "$EARLIER_DB" ] && [ "$LIVE_DB" != "$EARLIER_DB" ] || { echo "the earlier push should have a database of its own: '$EARLIER_DB' / '$LIVE_DB'"; exit 1; }
psql_q "$LIVE_DB" "CREATE TABLE after_third (id int)" >/dev/null
OWN_DATA="$(psql_q "$EARLIER_DB" "SELECT to_regclass('public.after_third') IS NULL") / $(psql_q "$LIVE_DB" "SELECT to_regclass('public.after_third') IS NOT NULL")"
[ "$OWN_DATA" = "t / t" ] || { echo "a table the preview's branch got after the push shouldn't be in the earlier push's copy (earlier absent / live present): $OWN_DATA"; exit 1; }
# Its Redis too: a database of its own.
redis_of() { curl -sS -b "$1" -H "Host: $2" "http://$API/env.txt" | sed -n 's/^REDIS_URL=//p'; }
LIVE_REDIS=$(redis_of "$JAR" "$HOST")
EARLIER_REDIS=$(redis_of "$EJAR" "$EARLIER_HOST")
[ -n "$EARLIER_REDIS" ] && [ "$EARLIER_REDIS" != "$LIVE_REDIS" ] || { echo "the earlier push should have a Redis database of its own: '$EARLIER_REDIS' / '$LIVE_REDIS'"; exit 1; }
OWN_DATA="$OWN_DATA; Redis its own"
# And its search cluster (a branch of the preview's, masked as it is): the cluster its data key
# names (`base64(<id>@<cluster>:<key>)`; every cluster answers at the node's one URL).
es_of() { curl -sS -b "$1" -H "Host: $2" "http://$API/env.txt" | sed -n 's/^ELASTICSEARCH_API_KEY=//p' | python3 -c "import base64,sys; k=sys.stdin.read().strip(); print(base64.b64decode(k).decode().split(':')[0].rsplit('@',1)[-1] if k else '')"; }
LIVE_ES=$(es_of "$JAR" "$HOST")
EARLIER_ES=$(es_of "$EJAR" "$EARLIER_HOST")
[ -n "$EARLIER_ES" ] && [ "$EARLIER_ES" != "$LIVE_ES" ] || { echo "the earlier push should have a search cluster of its own: '$EARLIER_ES' / '$LIVE_ES'"; exit 1; }
OWN_DATA="$OWN_DATA, search its own"
# And its bucket (a branch of the preview's, with a key of its own).
bucket_of() { curl -sS -b "$1" -H "Host: $2" "http://$API/env.txt" | sed -n 's/^S3_BUCKET=//p'; }
LIVE_BUCKET=$(bucket_of "$JAR" "$HOST")
EARLIER_BUCKET=$(bucket_of "$EJAR" "$EARLIER_HOST")
[ -n "$EARLIER_BUCKET" ] && [ "$EARLIER_BUCKET" != "$LIVE_BUCKET" ] || { echo "the earlier push should have a bucket of its own: '$EARLIER_BUCKET' / '$LIVE_BUCKET'"; exit 1; }
OWN_DATA="$OWN_DATA, bucket its own"
# And its stream (a branch of the preview's: its own SASL user).
stream_of() { curl -sS -b "$1" -H "Host: $2" "http://$API/env.txt" | sed -n 's/^KAFKA_SASL_USERNAME=//p'; }
LIVE_STREAM=$(stream_of "$JAR" "$HOST")
EARLIER_STREAM=$(stream_of "$EJAR" "$EARLIER_HOST")
[ -n "$EARLIER_STREAM" ] && [ "$EARLIER_STREAM" != "$LIVE_STREAM" ] || { echo "the earlier push should have a stream of its own: '$EARLIER_STREAM' / '$LIVE_STREAM'"; exit 1; }
OWN_DATA="$OWN_DATA, stream its own"
# And its NoSQL database: a branch of the preview's, still masked (it inherits the read policy).
nosql_of() { curl -sS -b "$1" -H "Host: $2" "http://$API/env.txt" | sed -n 's/^CASSANDRA_USERNAME=//p'; }
[ "$(nosql_of "$EJAR" "$EARLIER_HOST")" != "$(nosql_of "$JAR" "$HOST")" ] || { echo "the earlier push should have a NoSQL database of its own"; exit 1; }
NOSQL_COPY_MASK=$(api GET /v1/nosql/databases | jq_ "[x.get('masking', {}).get('app.users.email', '') for x in (d if isinstance(d, list) else d.get('databases', [])) if x['name'].startswith('pv-') and x['id'] != '$BRANCH_NOSQL'][0] if [x for x in (d if isinstance(d, list) else d.get('databases', [])) if x['name'].startswith('pv-') and x['id'] != '$BRANCH_NOSQL'] else 'no copy'")
[ "$NOSQL_COPY_MASK" = "$MASKED" ] || { echo "the earlier push's NoSQL copy should be masked as the preview's branch is ($MASKED), not '$NOSQL_COPY_MASK'"; exit 1; }
OWN_DATA="$OWN_DATA, NoSQL its own and masked"

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
# Its link to a page that went is checked (HEAD), and broken; its 404 isn't the page's failed request.
LINKS=$(api GET "/v1/previews/$PV" | jq_ "' '.join('%s %s' % (b['path'], b['status']) for b in (d.get('page_links') or {}).get('broken', []))")
[ "$LINKS" = "/gone 404" ] || { echo "the page's broken link should be found: $LINKS"; exit 1; }
# Against main's (which links nowhere): it's the change's.
# (The comparison with main comes just after the page's own checks, and needs main's own
# screenshot, which a loaded host takes minutes over: both waited for.)
for _ in $(seq 1 300); do
  [ "$(api GET "/v1/previews/$MAINPV" | jq_ "d.get('screenshot_at') is not None")" = True ] && break
  sleep 1
done
for _ in $(seq 1 240); do
  LINKS_NEW=$(api GET "/v1/previews/$PV" | jq_ "(d.get('base_change') or {}).get('links_added')")
  [ "$LINKS_NEW" = 1 ] && break
  sleep 0.5
done
[ "$LINKS_NEW" = 1 ] || {
  echo "the broken link should be new against main: $LINKS_NEW"
  echo "base_change: $(api GET "/v1/previews/$PV" | jq_ "{k: v for k, v in (d.get('base_change') or {}).items() if k in ('branch', 'head', 'errors_added', 'links_added', 'a11y_added', 'seo_added')}")"
  echo "main's page_links: $(api GET "/v1/previews?project=$PRJ" | jq_ "[(p.get('branch'), p.get('page_links'), p.get('screenshot_at')) for p in d if p.get('change') is None]")"
  echo "main's preview: $(api GET "/v1/previews/$MAINPV" | jq_ "{k: d.get(k) for k in ('state', 'error', 'head')}")"
  echo "main's activity: $(api GET "/v1/previews/$MAINPV/activity" | jq_ "[a['message'][:240] for a in d[:12]]")"
  exit 1
}
case "$HEALTH" in *"; 0 failed") ;; *) echo "the link check's own requests aren't the page's: $HEALTH"; exit 1;; esac
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
# Its words against main's: the change's line in, main's out.
WORDS=$(api GET "/v1/previews/$PV" | jq_ "(lambda t: '+%s -%s' % (t['added'], t['removed']))(d['base_change'].get('text') or {'added': [], 'removed': []})")
# Its API against main's, read from each one's OpenAPI document (github.openapi).
# (Compared again with each screenshot: until the comparison with this head's is in.)
for _ in $(seq 1 120); do
  API_DIFF=$(api GET "/v1/previews/$PV" | jq_ "(lambda t: '+%s -%s' % (t['added'], t['removed']))(d['base_change'].get('api') or {'added': [], 'removed': []})")
  [ "$API_DIFF" != "+[] -[]" ] && break
  sleep 0.5
done
[ "$API_DIFF" = "+['POST /orders'] -['DELETE /users/{id}']" ] || { echo "the change's API against main's should name the operation it removes and the one it adds: $API_DIFF"; exit 1; }
# And what it breaks inside an operation both keep: the limit parameter becomes required.
API_BREAKING=$(api GET "/v1/previews/$PV" | jq_ "' | '.join(d['base_change'].get('api_breaking') or [])")
[ "$API_BREAKING" = "GET /users: the parameter query:limit becomes required" ] || { echo "the change's API should name what it breaks in an operation both keep: $API_BREAKING"; exit 1; }
# Its endpoints timed on the preview's data, against main's (github.endpoints).
ENDPOINTS=$(api GET "/v1/previews/$PV" | jq_ "' '.join('%s %s %s ms, main %s ms' % (x['path'], x['status'], x['ms'], x['main_ms']) for x in d['base_change'].get('endpoints') or [])")
case "$ENDPOINTS" in "/openapi.json 200 "*" ms, main "[0-9]*" ms") ;; *) echo "the change's endpoints should be timed against main's: $ENDPOINTS"; exit 1;; esac
case "$WORDS" in "+['preview of change 1"*"-['main']") ;; *) echo "the page's words should be compared with main's: $WORDS"; exit 1;; esac
MAIN_LOAD=$(api GET "/v1/previews/$PV" | jq_ "d['base_change'].get('load_ms')")
case "$MAIN_LOAD" in [0-9]*) ;; *) echo "the comparison with main should carry main's load time: $MAIN_LOAD"; exit 1;; esac
# Main's page misses a language and a title too: none of that is the change's.
A11Y_NEW=$(api GET "/v1/previews/$PV" | jq_ "d['base_change'].get('a11y_added')")
# SEO basics from the same session: the bare test page has none of them, nor main's.
SEO=$(api GET "/v1/previews/$PV" | jq_ "(lambda s: 'description %s, viewport %s, h1 %s, noindex %s' % (s['description'], s['viewport'], s['h1'], s['noindex']))(d['page_a11y']['seo']) + '; new against main: %s' % d['base_change'].get('seo_added')")
[ "$SEO" = "description False, viewport False, h1 0, noindex False; new against main: 0" ] || { echo "the page's SEO basics should be read and compared with main's: $SEO"; exit 1; }
[ "$A11Y_NEW" = 0 ] || { echo "the change adds no accessibility problems main doesn't have: $A11Y_NEW"; exit 1; }
# Its page logs "boom" and main's doesn't: that one is the change's.
ERRORS_NEW=$(api GET "/v1/previews/$PV" | jq_ "d['base_change'].get('errors_added')")
case "$ERRORS_NEW" in [0-9]*) ;; *) echo "the comparison with main should count the change's own errors: $ERRORS_NEW"; exit 1;; esac
# The other page, taken in the same session and compared with main's (compared again with each
# screenshot: until the comparison with this head's, /pricing's checks against main's in it).
for _ in $(seq 1 120); do
  [ "$(api GET "/v1/previews/$PV" | jq_ "any(x['path'] == '/pricing' and x.get('against') is not None for x in d.get('pages', []))")" = True ] && break
  sleep 0.5
done
PAGES=$(api GET "/v1/previews/$PV" | jq_ "'; '.join('%s: %d error(s), %s%% against main' % (x['path'], x['errors'], x['base_percent']) for x in d.get('pages', []))")
case "$PAGES" in *"/pricing: "[1-9]*" error(s), "*"% against main"*) ;; *) echo "/pricing should be taken, with its 404, and compared with main's: $PAGES"; exit 1;; esac
# The first page's checks on /pricing too (the same 404 page as main's: nothing new against it).
PRICING=$(api GET "/v1/previews/$PV" | jq_ "(lambda p: 'a11y %s, seo %s; new: a11y %s, seo %s, links %s' % (p.get('a11y') is not None, ((p.get('a11y') or {}).get('seo')) is not None, (p.get('against') or {}).get('a11y_added'), (p.get('against') or {}).get('seo_added'), (p.get('against') or {}).get('links_added')))([x for x in d['pages'] if x['path'] == '/pricing'][0])")
[ "$PRICING" = "a11y True, seo True; new: a11y 0, seo 0, links 0" ] || { echo "/pricing should get the first page's checks, against main's /pricing: $PRICING"; exit 1; }
case "$PAGES" in *"wiki:/: 0 error(s), "*"% against main"*) ;; *) echo "the wiki service's page should be taken in its own run and compared: $PAGES"; exit 1;; esac
# A page state (github.states: Tab pressed on /), taken after its step and compared with main's.
case "$PAGES" in *"state:tab: "*"% against main"*) ;; *) echo "the page state should be taken after its steps and compared: $PAGES"; exit 1;; esac
# The repo's own page, from its roll.toml (pages = ["/index.html"]), taken with the project's.
case "$PAGES" in *"/index.html: "*"% against main"*) ;; *) echo "the repo's own page should be taken and compared: $PAGES"; exit 1;; esac
# And the repo's own, from its roll.toml ([[preview.states]], versioned with its selectors).
case "$PAGES" in *"state:repo-tab: "*"% against main"*) ;; *) echo "the repo's own page state should be taken and compared: $PAGES"; exit 1;; esac
# And in Firefox (github.browsers), compared with main's in Firefox.
case "$PAGES" in *"browser:firefox: "*"% against main"*) ;; *) echo "the first page should be taken in Firefox and compared: $PAGES"; exit 1;; esac
case "$PAGES" in *"browser:webkit-phone: "*"% against main"*) ;; *) echo "the first page should be taken in WebKit at a phone's size and compared: $PAGES"; exit 1;; esac
# The first page at a tablet's width too, compared with main's at that width.
case "$PAGES" in *"@768: "*"% against main"*) ;; *) echo "the first page should be taken at 768 px and compared: $PAGES"; exit 1;; esac
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
# A required review is a person's to accept, signed in to roll: never the operator's token (a CI job
# may hold it). The rest runs as a check, which the operator may accept.
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://$API/v1/previews/$PV/visual/accept" -H 'content-type: application/json' -d '{}')" = 403 ] || { echo "the operator's token shouldn't accept a required visual review"; exit 1; }
api PUT "/v1/previews/projects/$PRJ/settings" '{"visual_review":"check"}' >/dev/null
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
REVIEW=$(api POST "/v1/previews/$PV/visual/accept" "{\"head\":\"$SEEN\"}" | jq_ "'%s by %s' % (d['visual_review']['state'], d['visual_review']['by'])")
[ "$REVIEW" = "accepted by operator" ] || { echo "accepting should settle the review: $REVIEW"; exit 1; }
case "$(api GET "/v1/code/changes/$CHG" | jq_ "[b['kind'] for b in d.get('blockers', [])]")" in *visual*) echo "an accepted review shouldn't block landing"; exit 1;; esac
say "against main's preview, both running: a slower route (and the span its time went to) and a new error reach the record and the change"
api POST "/v1/previews/$MAINPV/wake" >/dev/null
api POST "/v1/previews/$PV/wake" >/dev/null
# main's service's OpenTelemetry endpoint, as its service has it (its own access link first).
MAIN_URL=$(api GET "/v1/previews/$MAINPV" | jq_ "[x for x in d['services'] if x['name'] == 'web'][0]['share_url']")
MAIN_HOST=$(echo "$MAIN_URL" | sed 's#https\?://##; s#[:/].*##')
MJAR="$HOME_DIR/main-cookies"
curl -sS -L -o /dev/null -c "$MJAR" -b "$MJAR" -H "Host: $MAIN_HOST" "http://$API/?${MAIN_URL#*\?}" --max-time 120
MAIN_OTEL=""
for _ in $(seq 1 120); do
  MAIN_OTEL=$(curl -sS -b "$MJAR" -H "Host: $MAIN_HOST" "http://$API/env.txt" | sed -n 's/^OTEL_EXPORTER_OTLP_ENDPOINT=//p' | sed 's#host.docker.internal#127.0.0.1#')
  [ -n "$MAIN_OTEL" ] && break
  sleep 1
done
[ -n "$MAIN_OTEL" ] || { echo "main's preview's service should have its OpenTelemetry endpoint"; exit 1; }
# Five requests each: GET /orders at 100 ms on main's (its query 50) and 400 ms here (its query 330).
post_orders() { # post_orders <endpoint> <ms> <query ms> <trace prefix>
  NOW_NS=$(date +%s%N) python3 - "$@" <<'PY'
import json, os, sys, urllib.request
ep, ms, q, pre = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
now = int(os.environ["NOW_NS"])
spans = []
for i in range(5):
    t = (pre * 32)[:28] + "%04x" % i
    spans.append({"traceId": t, "spanId": "%016x" % 1, "name": "GET /orders", "startTimeUnixNano": str(now - ms * 1000000), "endTimeUnixNano": str(now)})
    spans.append({"traceId": t, "spanId": "%016x" % 2, "parentSpanId": "%016x" % 1, "name": "SELECT orders", "startTimeUnixNano": str(now - q * 1000000), "endTimeUnixNano": str(now)})
body = json.dumps({"resourceSpans": [{"resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "web"}}]}, "scopeSpans": [{"spans": spans}]}]}).encode()
urllib.request.urlopen(urllib.request.Request(ep + "/v1/traces", body, {"content-type": "application/json"})).read()
PY
}
post_orders "$MAIN_OTEL" 100 50 a1
post_orders "$OTEL_HERE" 400 330 b2
# An error only the change's service writes.
docker exec "roll-previews-$(echo "$HOST" | cut -d. -f1)" sh -c 'echo "ERROR inventory reservation failed for the cart" > /proc/1/fd/2'
AGAINST=""
for _ in $(seq 1 90); do
  AGAINST=$(api GET "/v1/previews/$PV" | jq_ "' / '.join([', '.join('%s %s->%s %s' % (r['name'], r['main_p50_ms'], r['p50_ms'], (r.get('grew') or {}).get('span')) for r in (d.get('slower_routes') or {}).get('routes', [])), str(any('inventory reservation failed' in e['title'] for e in (d.get('new_errors') or {}).get('errors', [])))])")
  case "$AGAINST" in "GET /orders 100->400 SELECT orders / True") break;; esac
  sleep 2
done
[ "$AGAINST" = "GET /orders 100->400 SELECT orders / True" ] || { echo "the change should be slower than main's on GET /orders (in SELECT orders) and have a new error: $AGAINST"; exit 1; }
# The change's summary (its pull request's comment and status read it) has both.
SUMMED=$(api GET "/v1/code/changes/$CHG" | jq_ "(lambda e: '%s %s' % (bool((e.get('slower_routes') or {}).get('routes')), (e.get('new_errors') or {}).get('total', 0) > 0))((d.get('preview') or {}).get('environment') or {})")
[ "$SUMMED" = "True True" ] || { echo "the change's preview summary should carry them: $SUMMED"; exit 1; }
if [ "${STOP_AT:-}" = against ]; then
  echo "stopped against main's preview: project $PRJ, preview $PV, main's $MAINPV, change $CHG (run with --keep to leave the daemon up)"
  exit 0
fi
# Turned off, it goes.
OFF=$(api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"baseline":false}}'); echo "$OFF" | grep -q "\"baseline\":false" || { echo "turning the baseline off: $OFF"; exit 1; }
for _ in $(seq 1 120); do
  [ "$(api GET "/v1/previews?project=$PRJ" | jq_ "len(d)")" = 1 ] && break
  sleep 0.5
done
[ "$(api GET "/v1/previews?project=$PRJ" | jq_ "len(d)")" = 1 ] || { echo "with github.baseline off, main's preview should go"; exit 1; }
PHONE=$(curl -sS "http://$API/v1/previews/$PV/screenshot?view=phone" | python3 -c "import sys, struct; b=sys.stdin.buffer.read(); print(f'{struct.unpack(\">I\", b[16:20])[0]} px wide, PNG' if b[:4]==b'\x89PNG' else 'not a PNG')")
case "$PHONE" in "780 px wide, PNG") ;; *) echo "the phone-width screenshot should be 390 px at twice the density: $PHONE"; exit 1;; esac

say "the open note's element, cropped by the next head's screenshot run"
CROP_NOTE=""
for _ in $(seq 1 240); do
  CROP_NOTE=$(api GET "/v1/previews/$PV/notes" | jq_ "([n['id'] for n in d['notes'] if n.get('crop')] or [''])[0]")
  [ -n "$CROP_NOTE" ] && break
  sleep 0.5
done
[ -n "$CROP_NOTE" ] || { echo "the open note should get a picture of its element: $(api GET "/v1/previews/$PV/notes")"; exit 1; }
# Saved, then read (piped into head, curl can fail writing the rest: exit 23 under pipefail).
curl -sS -o "$HOME_DIR/crop.png" "http://$API/v1/previews/$PV/notes/$CROP_NOTE/crop"
CROP=$(head -c 8 "$HOME_DIR/crop.png" | od -An -tx1 | tr -d ' \n')
[ "$CROP" = 89504e470d0a1a0a ] || { echo "the note's crop should be a PNG: $CROP"; exit 1; }

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

say "idle past sleep_after, its service is frozen (paused), not stopped: the next visit thaws it"
SHARE2=$(api GET "/v1/previews/$PV" | jq_ "[s['share_url'] for s in d['services'] if s['name']=='web'][0]")
HOST2=$(echo "$SHARE2" | sed -E 's#^https?://##; s#/.*##; s#:.*##')
JAR2="$HOME_DIR/cookies-frozen"
curl -sS -o /dev/null -L -c "$JAR2" -b "$JAR2" -H "Host: $HOST2" "http://$API/?${SHARE2#*\?}" --max-time 120
CONT2="roll-previews-$(echo "$HOST2" | cut -d. -f1)"
for _ in $(seq 1 200); do
  [ "$(docker inspect -f '{{.State.Paused}}' "$CONT2" 2>/dev/null)" = true ] && break
  sleep 1
done
[ "$(docker inspect -f '{{.State.Paused}}' "$CONT2" 2>/dev/null)" = true ] || { echo "an idle service should be frozen (paused) after sleep_after"; docker ps -a --filter "name=$CONT2"; exit 1; }
FROZEN_STATE=$(api GET "/v1/previews/$PV" | jq_ "[s['state'] for s in d['services'] if s['name']=='web'][0]")
[ "$FROZEN_STATE" = sleeping ] || { echo "a frozen service should read as sleeping: $FROZEN_STATE"; exit 1; }
ID_FROZEN=$(docker inspect -f '{{.Id}}' "$CONT2")
T6=$(now_ms); BODY2=$(curl -sS -b "$JAR2" -H "Host: $HOST2" "http://$API/" --max-time 60); T7=$(now_ms)
echo "$BODY2" | grep -q "preview of change 1" || { echo "the thawed service should answer: $BODY2"; exit 1; }
[ "$(docker inspect -f '{{.Id}} {{.State.Running}} {{.State.Paused}}' "$CONT2")" = "$ID_FROZEN true false" ] || { echo "the visit should thaw the same container, not start another"; exit 1; }
[ $((T7 - T6)) -lt 1500 ] || { echo "a thaw should answer within 1.5 s, not $((T7 - T6)) ms"; exit 1; }

say "egress: roll puts the preview's services on a network of their own, with no route out"
REACH='import socket; socket.create_connection(("1.1.1.1", 443), 3); print("out")'
# Awake first (an idle service is frozen, and a frozen container takes no exec).
curl -sS -o /dev/null -b "$JAR2" -H "Host: $HOST2" "http://$API/" --max-time 60 || true
OUT_BEFORE=$(docker exec "$CONT2" python -c "$REACH" 2>&1 || true); OUT_BEFORE=$(echo "$OUT_BEFORE" | tail -1)
# The premise: by default a preview's service reaches the internet.
[ "$OUT_BEFORE" = out ] || { echo "a preview's service should reach the internet by default (egress open): $OUT_BEFORE"; exit 1; }
SHOT_BEFORE=$(api GET "/v1/previews/$PV" | jq_ "d.get('screenshot_at')")
api PUT "/v1/previews/projects/$PRJ/settings" '{"egress":"roll"}' >/dev/null
NETS=""
for _ in $(seq 1 120); do
  curl -sS -o /dev/null -b "$JAR2" -H "Host: $HOST2" "http://$API/" --max-time 60 || true
  NETS=$(docker inspect -f '{{range $n, $c := .NetworkSettings.Networks}}{{$n}} {{end}}' "$CONT2" 2>/dev/null || true)
  case "$NETS" in *roll-previews-net-*) break;; esac
  sleep 1
done
case "$NETS" in *roll-previews-net-*) ;; *) echo "with egress roll the service should run on the preview's own network: $NETS"; exit 1;; esac
BODY3=$(curl -sS -b "$JAR2" -H "Host: $HOST2" "http://$API/" --max-time 60)
echo "$BODY3" | grep -q "preview of change 1" || { echo "the service on its own network should still be served through roll: $BODY3"; exit 1; }
OUT_AFTER=$(docker exec "$CONT2" python -c "$REACH" 2>&1 || true); OUT_AFTER=$(echo "$OUT_AFTER" | tail -1)
case "$OUT_AFTER" in *unreachable*|*Errno*|*timed*) ;; *) echo "with egress roll the service shouldn't reach the internet: $OUT_AFTER"; exit 1;; esac
# Its screenshot is still taken there (the browser in the service's own network: nothing is
# published, the port comes from the container's label), at the change's next push.
api POST "/v1/code/changes/$CHG/revise" '{"base":"main","message":"Roll only","files":{"index.html":{"content":"preview of change 1, roll only"},"docs/setup.md":{"content":"how to run it"},'"$OPENAPI_CHG"'}}' >/dev/null
SHOT_ROLL=""
for _ in $(seq 1 240); do
  SHOT_ROLL=$(api GET "/v1/previews/$PV" | jq_ "d.get('screenshot_at')")
  [ "$SHOT_ROLL" != "$SHOT_BEFORE" ] && [ "$SHOT_ROLL" != None ] && break
  sleep 1
done
[ "$SHOT_ROLL" != "$SHOT_BEFORE" ] && [ "$SHOT_ROLL" != None ] || { echo "with egress roll the screenshot should still be taken: $SHOT_BEFORE then $SHOT_ROLL"; api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d if 'screenshot' in a['message']][:5]"; exit 1; }
# A host the project allows: reached through the preview's egress proxy, anything else refused
# and said in the activity.
# 169.254.169.254.nip.io is listed too: a name the project chose that points at the cloud's
# metadata address (a private one) is refused, never connected to.
api PUT "/v1/previews/projects/$PRJ/settings" '{"egress":"roll","egress_hosts":["github.com","169.254.169.254.nip.io"]}' >/dev/null
FETCH='import sys, urllib.request
try:
    print(urllib.request.urlopen(sys.argv[1], timeout=15).status)
except Exception as e:
    print(type(e).__name__, str(e)[:80])'
ALLOWED=""
for _ in $(seq 1 120); do
  curl -sS -o /dev/null -b "$JAR2" -H "Host: $HOST2" "http://$API/" --max-time 60 || true
  ALLOWED=$(docker exec "$CONT2" python -c "$FETCH" https://github.com/ 2>&1 || true); ALLOWED=$(echo "$ALLOWED" | tail -1)
  [ "$ALLOWED" = 200 ] && break
  sleep 1
done
[ "$ALLOWED" = 200 ] || { echo "an allowed host should be reached through the egress proxy: $ALLOWED"; exit 1; }
REFUSED=$(docker exec "$CONT2" python -c "$FETCH" https://example.com/ 2>&1 || true); REFUSED=$(echo "$REFUSED" | tail -1)
case "$REFUSED" in *403*|*Tunnel*) ;; *) echo "a host not in egress_hosts should be refused: $REFUSED"; exit 1;; esac
METADATA=$(docker exec "$CONT2" python -c "$FETCH" http://169.254.169.254.nip.io/latest/meta-data/ 2>&1 || true); METADATA=$(echo "$METADATA" | tail -1)
case "$METADATA" in *403*|*Forbidden*) ;; *) echo "a listed name that resolves to a private address should be refused: $METADATA"; exit 1;; esac
TOLD=""
for _ in $(seq 1 60); do
  TOLD=$(api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d if 'egress_hosts' in a.get('message','')][:2]")
  case "$TOLD" in *example.com*private*|*private*example.com*) break;; esac
  sleep 1
done
case "$TOLD" in *example.com*private*|*private*example.com*) ;; *) echo "the activity should say what the proxy refused, and the private address: $TOLD"; exit 1;; esac
api PUT "/v1/previews/projects/$PRJ/settings" '{"egress":"open","egress_hosts":[]}' >/dev/null

if [ -n "${STOP_BEFORE_LAND:-}" ]; then
  echo "stopped before landing: project $PRJ, preview $PV (run with --keep to leave the daemon up)"
  exit 0
fi
say "land the change: the teardown runs, the preview and its branches go, the parents stay"
# The project's teardown, as the preview goes: exit 3 only when the branch's URL reached it, and
# its failure doesn't keep the preview.
api PUT "/v1/previews/projects/$PRJ/settings" '{"teardown":{"image":"'"$IMAGE"'","run":"test -n \"$DATABASE_URL\" && exit 3"}}' >/dev/null
api POST "/v1/code/changes/$CHG/land" '{}' >/dev/null
for _ in $(seq 1 240); do
  N=$(api GET "/v1/previews?project=$PRJ" | jq_ "len(d)")
  [ "$N" = 0 ] && break
  sleep 0.5
done
[ "$N" = 0 ] || { api GET "/v1/previews?project=$PRJ"; exit 1; }
TEARDOWN=$(sed 's/\x1b\[[0-9;]*m//g' "$HOME_DIR/rolld.log" | grep -c "the teardown failed preview=$PV error=the teardown command failed with exit code 3" || true)
[ "$TEARDOWN" = 1 ] || { echo "the teardown should run once, with the branch's URL, as the preview goes: $TEARDOWN"; grep -i teardown "$HOME_DIR/rolld.log" | tail -5; exit 1; }
PARENTS_OK=$( (api GET "/v1/redis/databases/$REDIS" | jq_ "d['id']"; api GET /v1/storage/buckets/assets | jq_ "d['name']"; api GET "/v1/nosql/databases/$NOSQL" | jq_ "d['id']"; api GET "/v1/search/clusters/$SEARCH" | jq_ "d['id']") | wc -l)
SEARCH_BRANCH_GONE=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${ROLL_ADMIN_TOKEN:-}" "http://$API/v1/search/clusters/$BRANCH_SEARCH")
[ "$SEARCH_BRANCH_GONE" = 404 ] || { echo "the search branch should be gone after landing ($SEARCH_BRANCH_GONE)"; exit 1; }
# Nor an earlier push's copy of it.
BUCKETS_LEFT=$(api GET /v1/storage/buckets | jq_ "len([b for b in (d if isinstance(d, list) else d.get('buckets', [])) if b['name'].startswith('assets-')])")
[ "$BUCKETS_LEFT" = 0 ] || { echo "no bucket branch of the preview's (nor an earlier push's copy) should outlive it: $BUCKETS_LEFT"; exit 1; }
STREAMS_LEFT=$(api GET /v1/streams | jq_ "len([x for x in (d if isinstance(d, list) else d.get('streams', [])) if x['name'].startswith('pv-')])")
[ "$STREAMS_LEFT" = 0 ] || { echo "no stream of the preview's (nor an earlier push's copy) should outlive it: $STREAMS_LEFT"; exit 1; }
SEARCH_LEFT=$(api GET /v1/search/clusters | jq_ "len([c for c in (d if isinstance(d, list) else d.get('clusters', [])) if c['name'].startswith('pv-')])")
[ "$SEARCH_LEFT" = 0 ] || { echo "no search cluster of the preview's should outlive it: $SEARCH_LEFT"; exit 1; }
BRANCHES_LEFT=$(api GET /v1/redis/databases | jq_ "len([x for x in d if x['name'].startswith('pv-')])")
# The Postgres branch and every earlier push's copy of it went with the preview.
PG_LEFT=$(api GET "/v1/projects/$PG" | jq_ "len([b for b in d['branches'] if b['name'].startswith('pv-')])")
[ "$PG_LEFT" = 0 ] || { echo "no Postgres branch of the preview (its own or an earlier push's copy) should outlive it: $(api GET "/v1/projects/$PG" | jq_ "[b['name'] for b in d['branches']]")"; exit 1; }

cat <<EOF
| What | Measured |
| --- | --- |
| egress: roll: the service's network / 1.1.1.1:443 from it before and after / its screenshot | ${NETS% } / $OUT_BEFORE, then: ${OUT_AFTER:0:60} / taken |
| egress_hosts ["github.com", "169.254.169.254.nip.io"]: github.com / example.com / the metadata name, through the preview's egress proxy | $ALLOWED / ${REFUSED:0:60} / ${METADATA:0:40} |
| A note's element, cropped by the next head's screenshot run (a PNG) | $CROP_NOTE |
| A visit to a service frozen after sleep_after (it read as sleeping), instead of a cold start | $((T7 - T6)) ms, the same container |
| Change opened → preview ready ($(echo "$P" | jq_ "len(d['resources'])") branches made, the service deployed) | $((T1 - T0)) ms |
| The preview's bucket key: its branch / production's bucket | $OWN_LS / $PARENT_LS |
| … step by step (its timeline: data, setup, deploy, checks) | $STEPS |
| An earlier push's own copy of the data: a table made after it is absent there / present in the preview's; its Redis, search, bucket, stream and NoSQL | $OWN_DATA |
| What a follower of the events hears (\`GET /v1/previews/events\`, roll MCP's Events) | $EVENTS |
| Search: a sync of the parent's connector on the parent / asked of the branch (inherited, waits for adoption) | $PSTATE / $BSTATE |
| Parent bucket bytes / the branch's own bytes after writing 1 MiB / objects the branch sees | $PARENT_OBJECTS / $BRANCH_OBJECTS / $BRANCH_SEES |
| NoSQL branch's mask on app.users.email / masks on the parent | $MASKED / $PARENT_MASKED |
| NoSQL: app.users.email in the branch / a filter on the real address there | $NOSQL_READ / $NOSQL_FILTER |
| NoSQL: a mask on app.users_by_email's key, set / search: a string mask on a number field, set | $KEY_MASK / $NUM_MASK |
| What each branch hands the preview's code | $DATA |
| Core Web Vitals of the first page (the screenshot's load) | $VITALS |
| The first page's words against main's | $WORDS |
| Its API against main's (from each one's OpenAPI document) / what it breaks in the operations both keep | $API_DIFF / $API_BREAKING |
| Its endpoints against main's (the median of 10 GETs) | $ENDPOINTS |
| SEO basics of the first page | $SEO |
| /pricing's own checks (against main's /pricing) | $PRICING |
| The first page's broken links (HEAD only) / new against main | $LINKS / $LINKS_NEW |
| Visual review under a 5% visual threshold | $UNDER |
| X-Robots-Tag on the service's page / on the 401 without the link | $ROBOTS / $ROBOTS_LOCKED |
| Its request log through roll (count; newest; any query kept) | $REQS |
| What its running services use (\`…/metrics\`) | $METRICS |
| The release's time on the preview's data (at its head) / on the change | $RELEASE / $CHG_RELEASE ms |
| The preview's link as a QR code (PNG, no-store) | $QR |
| Mail: the provider's key in the service / sent as the preview, caught | gone / $MAIL |
| A page's note: its reply, listed under it on the page | $REPLIES |
| A page's note resolved in roll: notes the page lists after | $RESOLVED |
| \`roll preview connect --kind postgres\`: the branch's own role | $(role "$CONNECT") |
| Postgres users.email in the branch / in the parent | $PG_MASKED / $PG_PARENT |
| The migration's risks the preview names | ${RISKS:-} |
| The teardown as the preview went (its exit code, with the branch's URL) / runs | 3 / $TEARDOWN |
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
| Search: the branch's data-only ELASTICSEARCH_API_KEY reads / makes a key | $KEY_READ / $KEY_MINT |
| Search: an address in an array of objects / under a dotted key / in an index named logs.2024, in the branch | $ES_DEEP / $ES_DOTTED |
| After landing: parents present / preview Redis branches left / Postgres branches left / search clusters left / bucket branches left / streams left (earlier pushes' copies too) | $PARENTS_OK of 4 / $BRANCHES_LEFT / $PG_LEFT / $SEARCH_LEFT / $BUCKETS_LEFT / $STREAMS_LEFT |
EOF
