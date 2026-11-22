#!/usr/bin/env bash
# roll previews driving roll apps, end to end (docs/previews.md, "Measured on this machine"). A
# repo with a Dockerfile is a roll apps service; a change gets an ephemeral environment pv-<id>
# deployed at its commit with the branched Redis URL; a preview's own variable reaches that
# environment and, removed, leaves it; landing removes it all. Needs Docker (roll apps builds the
# commit). Its own rolld (home ~/.cache/roll-previews-apps-e2e, ports <P>xxx, 28 unless
# PREVIEW_APPS_PORTS says another two digits).
#
#   cargo build -p rolld -p roll-cli && scripts/preview-apps.sh
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${ROLLD:-$ROOT/target/debug/rolld}
CLI=${ROLL_CLI:-$ROOT/target/debug/roll}
H=$HOME/.cache/roll-previews-apps-e2e
PORTS=${PREVIEW_APPS_PORTS:-28}
API=127.0.0.1:${PORTS}070
say() { printf '\033[2m%s\033[0m\n' "$*" >&2; }
api() { local m=$1 p=$2 b=${3:-}; if [ -n "$b" ]; then curl -sS -X "$m" "http://$API$p" -H 'content-type: application/json' -d "$b"; else curl -sS -X "$m" "http://$API$p"; fi; }
jq_() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {'d': d}))" "$1"; }
now_ms() { date +%s%3N; }
SVC=""
SVC2=""
SVC3=""
GITD=""
PRJ=""
cleanup() {
  [ -n "${PID:-}" ] && { kill "$PID" 2>/dev/null || true; for _ in $(seq 1 20); do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done; kill -9 "$PID" 2>/dev/null || true; }
  for c in $(docker ps -aq --filter "label=sh.roll.code.home=$H"); do docker rm -f "$c" >/dev/null; done
  [ -n "$GITD" ] && kill "$GITD" 2>/dev/null
  [ -n "${GHD:-}" ] && kill "$GHD" 2>/dev/null
  for one in $SVC $SVC2 $SVC3; do
    s=${one#svc_}
    for c in $(docker ps -aq --filter "name=^roll-apps-$s-"); do docker rm -f "$c" >/dev/null; done
    for i in $(docker images --format '{{.Repository}}:{{.Tag}}' | grep "^roll-apps-$s:" || true); do docker rmi -f "$i" >/dev/null || true; done
    for v in $(docker volume ls -q | grep "^roll-apps-vol-$s-" || true); do docker volume rm -f "$v" >/dev/null || true; done
  done
  if [ -n "$PRJ" ]; then
    # A closed environment's egress proxy, and the project's closed builder (buildx, named as its
    # network) with its proxy: left running when rolld stops before the environment's delete.
    # The closed build's own egress proxy too: still attached, it keeps the build network from
    # going, and leaked networks use up Docker's address pools (closed deploys then fail).
    for c in $(docker ps -aq --filter "name=roll-apps-egress-closed-${PRJ#prj_}" --filter "name=roll-apps-build-${PRJ#prj_}" --filter "name=roll-apps-egress-build-${PRJ#prj_}"); do docker rm -f "$c" >/dev/null; done
    docker buildx rm -f "roll-apps-build-${PRJ#prj_}" >/dev/null 2>&1 || true
    docker network rm "roll-apps-build-${PRJ#prj_}" >/dev/null 2>&1 || true
    # roll apps' link relays (`roll-apps-link-…`) stay attached to the project's networks and kept
    # every run's networks from going: 23 had piled up and Docker's address pools ran out.
    for c in $(docker ps -aq --filter "name=^roll-apps-link-${PRJ#prj_}-" --filter "name=^roll-apps-link-closed-${PRJ#prj_}."); do docker rm -f "$c" >/dev/null; done
    for n in $(docker network ls --format '{{.Name}}' | grep -E "^roll-apps-(net-${PRJ#prj_}-|closed-${PRJ#prj_}\.)" || true); do docker network rm "$n" >/dev/null || true; done
  fi
}
trap cleanup EXIT
if curl -s -o /dev/null --max-time 1 "http://$API/v1/info"; then echo "something answers on $API" >&2; exit 1; fi
rm -rf "$H" && mkdir -p "$H"
# roll's GitHub App, pointed at a fake GitHub (scripts/lib/fake-github.py) whose one installation
# sees acme/shop, served by git's own smart HTTP on GIT_PORT (the App's web host).
GIT_PORT=${PORTS}417
GH_PORT=${PORTS}418
openssl genrsa -traditional -out "$H/gh.pem" 2048 2>/dev/null
python3 "$ROOT/scripts/lib/fake-github.py" "$GH_PORT" >"$H/fake-github.log" 2>&1 &
GHD=$!
export ROLL_GITHUB_APP_ID=1 ROLL_GITHUB_PRIVATE_KEY_PATH=$H/gh.pem ROLL_GITHUB_WEBHOOK_SECRET=live-check ROLL_GITHUB_APP_SLUG=roll \
  ROLL_GITHUB_API_URL=http://127.0.0.1:$GH_PORT ROLL_GITHUB_WEB_URL=http://127.0.0.1:$GIT_PORT
export ROLL_HOME=$H ROLL_API=$API ROLL_PROXY=127.0.0.1:${PORTS}432 ROLL_REDIS=127.0.0.1:${PORTS}380 ROLL_REDIS_HTTP=127.0.0.1:${PORTS}081 ROLL_KAFKA=127.0.0.1:${PORTS}092 \
  ROLL_APPS_LISTEN=127.0.0.1:${PORTS}080 ROLL_APPS_DOMAIN=apps.localhost ROLL_CODE_PREVIEW_DOMAIN=preview.localhost \
  ROLL_PRODUCTS=code,redis,streams,apps ROLL_APPS_ALLOW_PRIVATE=1 RUST_LOG=${RUST_LOG:-info}
unset ROLL_ADMIN_TOKEN ROLL_CODE_TOKEN
# A deployment key, as a real node has: a preview's own variables are kept sealed with it.
export ROLL_SECRET_KEY=${PREVIEW_APPS_SECRET_KEY:-$(python3 -c 'import secrets; print(secrets.token_hex(32))')}
nice -n 10 "$BIN" >"$H/rolld.log" 2>&1 &
PID=$!
for _ in $(seq 1 120); do kill -0 "$PID" 2>/dev/null || { tail -20 "$H/rolld.log"; exit 1; }; curl -sf "http://$API/v1/info" >/dev/null 2>&1 && break; sleep 0.5; done

say "a repo with a Dockerfile, a Redis database, a project, a roll apps service"
api POST /v1/code/owners '{"name":"acme","kind":"org"}' >/dev/null
api POST /v1/code/repos '{"owner":"acme","name":"web","source":"native"}' >/dev/null
BODY_INIT=$(python3 - <<'PY'
import json
start = "#!/bin/sh\necho \"$REDIS_URL\" | sed -E 's#.*default:(rdb_[a-z0-9]+)_.*#\\1#' > where\nprintf '%s' \"$KAFKA_USERNAME\" > kafka_user\nprintf '%s' \"$KAFKA_BROKERS\" > kafka_brokers\nexec python -m http.server ${PORT:-8000}\n"
docker = 'FROM python:3.13-alpine\nWORKDIR /app\nCOPY . .\nCMD ["sh", "start.sh"]\n'
# What previews' screenshots of the service take, in roll apps' own config ([app.previews.screenshots]).
toml = '[app.previews.screenshots]\n[[app.previews.screenshots.states]]\nname = "apps-tab"\npage = "/"\nsteps = [{ press = "Tab" }]\n'
print(json.dumps({"message": "init", "files": {"Dockerfile": {"content": docker}, "start.sh": {"content": start}, "index.html": {"content": "main"}, "roll.toml": {"content": toml}}}))
PY
)
api POST /v1/code/repos/acme/web/land "$BODY_INIT" >/dev/null
REDIS=$(api POST /v1/redis/databases '{"name":"cache"}' | jq_ "d['id']")
STREAM=$(api POST /v1/streams '{"name":"clicks"}' | jq_ "d['id']")
PRJ=$(api POST /v1/workspace/projects '{"name":"shop"}' | jq_ "d['id']")
for r in "repo/acme%2Fweb" "redis/$REDIS" "stream/$STREAM"; do api PUT "/v1/workspace/projects/$PRJ/resources/$r" '{"x":0,"y":0}' >/dev/null; done
SVCJ=$(api POST "/v1/apps/projects/$PRJ/services" '{"name":"web","source":{"kind":"repo","repo":"acme/web"},"run":{"type":"web","port":8000}}')
SVC=$(echo "$SVCJ" | jq_ "d['id']")
# A stream linked to it: production's link gets a SASL user of its own (KAFKA_BROKERS, …).
api PUT "/v1/apps/services/$SVC/links/stream/$STREAM" '{}' >/dev/null
say "service $SVC"
api PUT "/v1/previews/projects/$PRJ/settings" '{"enabled":true,"access":"public"}' >/dev/null

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
[ -n "$PV" ] || { echo "$L"; grep -i "preview" "$H/rolld.log" | tail -20; exit 1; }
T1=$(now_ms)
P=$(api GET "/v1/previews/$PV")
ENV="pv-${PV#pv_}"
URL=$(echo "$P" | jq_ "d['services'][0]['url']")
say "preview $PV ready in $((T1 - T0)) ms, service url $URL"
E=$(api GET "/v1/apps/services/$SVC/environments/$ENV")
echo "$E" | python3 -m json.tool | head -30 >&2
# Wait for the deploy to be active, then open it.
for _ in $(seq 1 600); do
  E=$(api GET "/v1/apps/services/$SVC/environments/$ENV")
  ST=$(echo "$E" | jq_ "(d.get('latest') or {}).get('status','')")
  [ "$ST" = active ] && break
  [ "$ST" = failed ] && { api GET "/v1/apps/services/$SVC/environments/$ENV/deploys" | head -c 2000; exit 1; }
  sleep 1
done
T2=$(now_ms)
VARS=$(api GET "/v1/apps/services/$SVC/environments/$ENV/variables" | jq_ "[v['name'] for v in d['vars']]")
# What previews sets there is sealed: roll apps never returns it (its logs hide it too).
PV_VALUES=$(api GET "/v1/apps/services/$SVC/environments/$ENV/variables/values")
[ -z "$(echo "$PV_VALUES" | jq_ "d.get('KAFKA_USERNAME', '')")" ] || { echo "previews' variables in a pv- environment should be sealed"; exit 1; }
# Its mail goes to roll's catcher, as on roll's own runtime (and providers' keys are replaced).
case "$VARS" in *"'SMTP_URL'"*"'SMTP_USER'"*) ;; *) echo "a pv- environment should get roll's SMTP: $VARS"; exit 1;; esac
case "$VARS" in *"'SENDGRID_API_KEY'"*) ;; *) echo "a pv- environment should get the providers' keys replaced: $VARS"; exit 1;; esac
HOSTPART=$(echo "$URL" | sed -E 's#^https?://##; s#/.*##; s#:.*##')
T3=$(now_ms)
BODY=$(curl -sS -H "Host: $HOSTPART" "http://127.0.0.1:${PORTS}080/" --max-time 120)
WHERE=$(curl -sS -H "Host: $HOSTPART" "http://127.0.0.1:${PORTS}080/where" --max-time 30)
T4=$(now_ms)
[ "$BODY" = "preview of change 1" ] || { echo "the preview should serve the change's page: $BODY"; exit 1; }
# The stream link's variables, as the instance got them, are the branch's under roll apps' names:
# its brokers and its own SASL user, never production's link's.
BRANCH_ENV=$(api GET "/v1/previews/$PV/connect" | python3 -c "import json,sys; d=json.load(sys.stdin); print(json.dumps(next((b['vars'] for b in d['branches'] if b['kind'] == 'stream'), {})))")
KAFKA_PV=$(curl -sS -H "Host: $HOSTPART" "http://127.0.0.1:${PORTS}080/kafka_user" --max-time 30)
KAFKA_BRANCH=$(echo "$BRANCH_ENV" | jq_ "d.get('KAFKA_SASL_USERNAME', '')")
[ -n "$KAFKA_PV" ] && [ "$KAFKA_PV" = "$KAFKA_BRANCH" ] || { echo "the stream link's KAFKA_USERNAME should be the branch's ($KAFKA_BRANCH), not '$KAFKA_PV'"; echo "$BRANCH_ENV" | head -c 600; exit 1; }
[ "$(curl -sS -H "Host: $HOSTPART" "http://127.0.0.1:${PORTS}080/kafka_brokers" --max-time 30)" = "$(echo "$BRANCH_ENV" | jq_ "d.get('KAFKA_BOOTSTRAP_SERVERS', '')")" ] || { echo "the stream link's KAFKA_BROKERS should be the branch's"; exit 1; }
# Kept out of search engines on roll apps too (roll apps 3b7cd1ac5, #26).
APPS_ROBOTS=$(curl -sS -o /dev/null -D - -H "Host: $HOSTPART" "http://127.0.0.1:${PORTS}080/" --max-time 30 | tr -d '\r' | sed -n 's/^x-robots-tag: //Ip')
[ "$APPS_ROBOTS" = noindex ] || { echo "a pv- environment's page should carry X-Robots-Tag: noindex, not '$APPS_ROBOTS'"; exit 1; }

say "Roll only on roll apps (apps' #143): the pv- instance reaches nothing but its listed hosts"
OPEN_DEP=$(api GET "/v1/apps/services/$SVC/environments/$ENV" | jq_ "(d.get('active') or {}).get('id','')")
api PUT "/v1/previews/projects/$PRJ/settings" '{"egress":"roll","egress_hosts":["example.com"]}' >/dev/null
api POST "/v1/previews/$PV/refresh" >/dev/null
# A deployment made closed, live: not the open one still answering while it's made.
for _ in $(seq 1 600); do
  E=$(api GET "/v1/apps/services/$SVC/environments/$ENV")
  [ "$(echo "$E" | jq_ "(d.get('egress') or {}).get('mode')")" = roll ] && [ "$(echo "$E" | jq_ "(d.get('active') or {}).get('id','')")" != "$OPEN_DEP" ] && [ "$(echo "$E" | jq_ "(d.get('latest') or {}).get('status','')")" = active ] && break
  sleep 1
done
[ "$(echo "$E" | jq_ "(d.get('active') or {}).get('id','')")" != "$OPEN_DEP" ] || { echo "the pv- environment should be deployed again closed: $(echo "$E" | jq_ "d.get('latest')"); see rolld.log"; exit 1; }
[ "$(echo "$E" | jq_ "(d.get('egress') or {}).get('mode')")" = roll ] || { echo "the pv- environment should be closed: $(echo "$E" | jq_ "d.get('egress')")"; exit 1; }
# One voice on the pull request: previews reports the pv- environment, roll apps doesn't (#249).
[ "$(echo "$E" | jq_ "d.get('report')")" = False ] || { echo "the pv- environment should leave reporting to previews (report: false): $(echo "$E" | jq_ "d.get('report')")"; exit 1; }
# Its keys bound to hosts are used as the rules' writer (closed_by, apps' #610): the operator
# saved these rules, so no one, and they're withheld (without previews' field, roll apps would
# name whoever set the egress: the operator).
[ "$(echo "$E" | jq_ "d.get('closed_by', 'absent')")" = None ] || { echo "the closed pv- environment should name no one as its placeholders' maker (closed_by: null): $(echo "$E" | jq_ "d.get('closed_by', 'absent')")"; exit 1; }
curl -sS -o /dev/null -H "Host: $HOSTPART" "http://127.0.0.1:${PORTS}080/" --max-time 120
INST=""
# The closed deployment's instance (its id's first six characters are in the name).
CLOSED=$(echo "$E" | jq_ "d['active']['id'][len('dep_'):][:6]")
for _ in $(seq 1 60); do INST=$(docker ps --format '{{.Names}}' | grep "^roll-apps-${SVC#svc_}-$ENV-$CLOSED-" | head -1); [ -n "$INST" ] && break; sleep 1; done
EGRESS_APPS=$(docker exec "$INST" python3 -c '
import socket, urllib.request
r = []
try:
    socket.create_connection(("1.1.1.1", 443), 3); r.append("1.1.1.1 reached")
except OSError:
    r.append("1.1.1.1 unreachable")
for host, want in (("example.com", "listed"), ("github.com", "unlisted")):
    try:
        urllib.request.urlopen("https://%s/" % host, timeout=20); r.append("%s %s through" % (want, host))
    except Exception:
        r.append("%s %s refused" % (want, host))
print(", ".join(r))')
[ "$EGRESS_APPS" = "1.1.1.1 unreachable, listed example.com through, unlisted github.com refused" ] || { echo "a closed pv- instance should reach only its listed hosts: $EGRESS_APPS"; exit 1; }
# What its proxy refused reaches the preview's activity (roll apps' egress_refused).
for _ in $(seq 1 120); do api GET "/v1/previews/$PV/activity" | grep -q "github.com" && break; sleep 1; done
api GET "/v1/previews/$PV/activity" | grep -q "github.com" || { echo "the refused host should be in the preview's activity"; exit 1; }
EGRESS_APPS="$EGRESS_APPS; said in its activity"
api PUT "/v1/previews/projects/$PRJ/settings" '{"egress":"open","egress_hosts":[]}' >/dev/null

say "a preview's own variable reaches its roll apps environment, and leaves it once removed"
apps_vars() { api GET "/v1/apps/services/$SVC/environments/$ENV/variables" | jq_ "' '.join(sorted(v['name'] for v in d['vars']))"; }
until_vars() { # until_vars <python test on the names, as n>
  local cond=$1
  for _ in $(seq 1 240); do
    NAMES=$(apps_vars)
    python3 -c "import sys; n=sys.argv[1].split(); sys.exit(0 if ($cond) else 1)" "$NAMES" && return 0
    sleep 1
  done
  echo "the environment's variables should satisfy $cond: $NAMES"
  api GET "/v1/previews/$PV/activity" | jq_ "[(a['at'] % 1000000, a['message']) for a in d[:12]]"
  api GET "/v1/previews/$PV" | jq_ "{k: d[k] for k in ('state', 'error')}"
  api GET "/v1/apps/services/$SVC/environments/$ENV/variables/values" | jq_ "d.get('ROLL_PREVIEW_SET')"
  api GET "/v1/apps/services/$SVC/environments/$ENV/deploys" | jq_ "[(x.get('id'), x.get('status'), x.get('trigger'), x.get('created_at', 0) % 1000000) for x in (d if isinstance(d, list) else d.get('deploys', []))[:6]]"
  grep -E "previews|PATCH|variables" "$H/rolld.log" | tail -12 | cut -c1-260
  exit 1
}
ROLL_API_URL="http://$API" "$CLI" preview env "$PV" FEATURE_FLAG=on >/dev/null
until_vars "'FEATURE_FLAG' in n and 'ROLL_PREVIEW_SET' in n"
ROLL_API_URL="http://$API" "$CLI" preview env "$PV" --unset FEATURE_FLAG >/dev/null
until_vars "'FEATURE_FLAG' not in n and 'REDIS_URL' in n"
UNSET="FEATURE_FLAG set, then unset; REDIS_URL kept"

say "traces: the project's traces turn on its pv- environment's own (roll apps keeps them), read from the preview"
api PUT "/v1/previews/projects/$PRJ/settings" '{"traces":true}' >/dev/null
for _ in $(seq 1 240); do
  [ "$(api GET "/v1/apps/services/$SVC/environments/$ENV" | jq_ "d.get('traces')")" = True ] && break
  sleep 1
done
[ "$(api GET "/v1/apps/services/$SVC/environments/$ENV" | jq_ "d.get('traces')")" = True ] || { echo "the pv- environment should get roll apps' traces"; exit 1; }
[ "$(api GET "/v1/previews/$PV/traces" | jq_ "type(d['traces']).__name__")" = list ] || { echo "the preview's traces should read its roll apps environment's"; exit 1; }
api PUT "/v1/previews/projects/$PRJ/settings" '{"traces":false}' >/dev/null

say "a screenshot of the roll apps service (in its instance's network), and main's baseline on roll apps"
api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"screenshot":true}}' >/dev/null
for _ in $(seq 1 240); do
  [ "$(api GET "/v1/previews/$PV" | jq_ "d.get('screenshot_at') is not None")" = True ] && break
  sleep 1
done
SHOT=$(curl -sS "http://$API/v1/previews/$PV/screenshot" | python3 -c "import sys; b=sys.stdin.buffer.read(); print(f'{len(b)} bytes, PNG' if b[:4]==b'\x89PNG' else 'not a PNG: ' + b[:200].decode('utf-8','replace'))")
case "$SHOT" in *PNG) ;; *) echo "the roll apps service should be screenshotted: $SHOT"; api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d[:8]]"; exit 1;; esac
# Its state from roll apps' own config ([app.previews.screenshots]), read from the change's tree.
APPS_STATE=$(api GET "/v1/previews/$PV" | jq_ "[p['path'] for p in d.get('pages', []) if p['path'].startswith('state:')]")
[ "$APPS_STATE" = "['state:apps-tab']" ] || { echo "the roll apps service's [app.previews.screenshots] state should be taken: $APPS_STATE"; exit 1; }
LOAD=$(api GET "/v1/previews/$PV" | jq_ "(d.get('page_speed') or {}).get('now', {}).get('load_ms')")
BASE=""
for _ in $(seq 1 600); do
  BASE=$(api GET "/v1/previews/$PV" | jq_ "('%s%% against %s' % (d['base_change']['percent'], d['base_change']['branch'])) if d.get('base_change') else ''")
  [ -n "$BASE" ] && break
  sleep 1
done
[ -n "$BASE" ] || { echo "main's baseline on roll apps should be compared with the change"; api GET "/v1/previews?project=$PRJ" | jq_ "[(p['branch'], p['state'], p.get('screenshot_at')) for p in d]"; exit 1; }
api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"screenshot":false}}' >/dev/null

say "the feedback button on the roll apps environment, for a visitor through roll (link access)"
api PUT "/v1/previews/projects/$PRJ/settings" '{"access":"link"}' >/dev/null
for _ in $(seq 1 240); do
  [ "$(api GET "/v1/apps/services/$SVC/environments/$ENV" | jq_ "d.get('feedback')")" = True ] && break
  sleep 1
done
[ "$(api GET "/v1/apps/services/$SVC/environments/$ENV" | jq_ "d.get('feedback')")" = True ] || { echo "the preview should ask roll apps for the feedback button"; exit 1; }
SHARE=$(api GET "/v1/previews/$PV" | jq_ "d['services'][0]['share_url']")
JAR="$H/apps-cookies"
FB_BODY=""
for _ in $(seq 1 120); do
  FB_BODY=$(curl -sS -L -c "$JAR" -b "$JAR" -H "Host: $HOSTPART" -H "Sec-Fetch-Dest: document" "http://127.0.0.1:${PORTS}080/?${SHARE#*\?}" --max-time 60)
  case "$FB_BODY" in *"/__roll/feedback.js"*) break;; esac
  sleep 1
done
case "$FB_BODY" in *"/__roll/feedback.js"*) ;; *) echo "a visitor through roll should get the feedback button: $FB_BODY"; exit 1;; esac
NOTES=$(curl -sS -b "$JAR" -H "Host: $HOSTPART" "http://127.0.0.1:${PORTS}080/__roll/feedback.json?path=%2F")
case "$NOTES" in "["*) ;; *) echo "the page's notes should come from roll previews: $NOTES"; exit 1;; esac
api PUT "/v1/previews/projects/$PRJ/settings" '{"access":"public"}' >/dev/null
# Public again redeploys the environment (no feedback button, no access check): settled before
# the promote reads which deployment runs, else the one it saw is replaced meanwhile (a 409).
for _ in $(seq 1 600); do
  [ "$(api GET "/v1/apps/services/$SVC/environments/$ENV" | jq_ "not d.get('feedback') and (d.get('latest') or {}).get('status') == 'active' and (d.get('latest') or {}).get('id') == (d.get('active') or {}).get('id')")" = True ] && break
  sleep 1
done
BRANCH=$(echo "$P" | jq_ "[r['ref'] for r in d['resources'] if r['kind']=='redis'][0]")

say "promote the preview's build to production (the sheet's Promote: roll apps' promote, its image, no build)"
# Production first runs main (its own deploy from the repo).
for _ in $(seq 1 600); do
  [ "$(api GET "/v1/apps/services/$SVC/environments/production" | jq_ "(d.get('latest') or {}).get('status','')")" = active ] && break
  sleep 1
done
PROD_HOST=$(api GET "/v1/apps/services/$SVC/environments/production" | jq_ "d.get('url') or ''" | sed -E 's#^https?://##; s#/.*##; s#:.*##')
PROD_BEFORE=$(curl -sS -H "Host: $PROD_HOST" "http://127.0.0.1:${PORTS}080/" --max-time 120)
[ "$PROD_BEFORE" = main ] || { echo "production should serve main before the promote: $PROD_BEFORE"; exit 1; }
# The deployment the sheet showed (`expect`): another one is refused (409), that one promoted.
SEEN=$(api GET "/v1/apps/services/$SVC/environments/$ENV" | jq_ "(d.get('active') or {}).get('id','')")
STALE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://$API/v1/apps/services/$SVC/environments/production/promote" -H 'content-type: application/json' -d "{\"from\":\"$ENV\",\"expect\":\"dep_not_what_runs\"}")
[ "$STALE" = 409 ] || { echo "a promote expecting another deployment should be refused: $STALE"; exit 1; }
PROMOTE=$(api POST "/v1/apps/services/$SVC/environments/production/promote" "{\"from\":\"$ENV\",\"expect\":\"$SEEN\"}")
PROMOTED=$(echo "$PROMOTE" | jq_ "d.get('trigger')")
[ "$PROMOTED" = promote ] || { echo "the promote should be a deploy of its own: $PROMOTE"; exit 1; }
T5=$(now_ms)
for _ in $(seq 1 300); do
  [ "$(api GET "/v1/apps/services/$SVC/environments/production" | jq_ "(d.get('latest') or {}).get('status','')")" = active ] && break
  sleep 1
done
T6=$(now_ms)
PROD_AFTER=$(curl -sS -H "Host: $PROD_HOST" "http://127.0.0.1:${PORTS}080/" --max-time 120)
[ "$PROD_AFTER" = "preview of change 1" ] || { echo "production should serve the preview's build after the promote: $PROD_AFTER"; exit 1; }
say "land"
api POST "/v1/code/changes/$CHG/land" '{}' >/dev/null
for _ in $(seq 1 120); do N=$(api GET "/v1/previews?project=$PRJ" | jq_ "len(d)"); [ "$N" = 0 ] && break; sleep 0.5; done
GONE=$(curl -s -o /dev/null -w '%{http_code}' "http://$API/v1/apps/services/$SVC/environments/$ENV")

say "a Git URL service: a push to a branch no environment follows makes its preview, the next push moves it, deleting the branch deletes it"
# The Git URL is roll code's own git over HTTP (a private address, so ROLL_APPS_ALLOW_PRIVATE).
api POST /v1/code/repos '{"owner":"acme","name":"site","source":"native"}' >/dev/null
SITE_INIT=$(python3 - <<'PY'
import json
docker = 'FROM python:3.13-alpine\nWORKDIR /app\nCOPY . .\nCMD ["python", "-m", "http.server", "8000"]\n'
print(json.dumps({"message": "init", "files": {"Dockerfile": {"content": docker}, "index.html": {"content": "site main"}}}))
PY
)
api POST /v1/code/repos/acme/site/land "$SITE_INIT" >/dev/null
GIT_URL="http://$API/git/acme/site.git"
SVC2J=$(api POST "/v1/apps/projects/$PRJ/services" "{\"name\":\"site\",\"source\":{\"kind\":\"git\",\"url\":\"$GIT_URL\"},\"run\":{\"type\":\"web\",\"port\":8000}}")
SVC2=$(echo "$SVC2J" | jq_ "d['id']") || { echo "a Git URL service should be made: $SVC2J"; exit 1; }
HOOK=$(api POST "/v1/apps/services/$SVC2/environments/production/hook" | jq_ "d['path']")
WT="$H/site-wt"
git clone -q "$GIT_URL" "$WT"
gitc() { git -C "$WT" -c user.name=e2e -c user.email=e2e@example.invalid "$@"; }
hook_push() { # hook_push <sha> <message>: GitHub's push event for the branch feature
  curl -sS -X POST "http://$API$HOOK" -H 'content-type: application/json' -H 'x-github-event: push' \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"ref": "refs/heads/feature", "after": sys.argv[1], "head_commit": {"message": sys.argv[2]}}))' "$1" "$2")"
}
git_preview() { api GET "/v1/previews?project=$PRJ" | jq_ "[p for p in d if p['branch'] == 'feature']"; }
serves() { # serves <body>: the feature branch's preview of the site serves it
  local want=$1 got=""
  for _ in $(seq 1 600); do
    GPV=$(api GET "/v1/previews?project=$PRJ" | jq_ "([p['id'] for p in d if p['branch'] == 'feature' and p['state'] in ('ready', 'sleeping')] + [''])[0]")
    if [ -n "$GPV" ]; then
      GURL=$(api GET "/v1/previews/$GPV" | jq_ "([s['url'] for s in d['services'] if s.get('url') and s['repo'] == 'git:$SVC2'] + [''])[0]")
      GHOST=$(echo "$GURL" | sed -E 's#^https?://##; s#/.*##; s#:.*##')
      [ -n "$GHOST" ] && got=$(curl -sS -H "Host: $GHOST" "http://127.0.0.1:${PORTS}080/" --max-time 30 2>/dev/null || true)
      [ "$got" = "$want" ] && return 0
    fi
    sleep 1
  done
  echo "the feature branch's preview should serve '$want', not '$got'"
  git_preview
  [ -n "${GPV:-}" ] && api GET "/v1/previews/$GPV/activity" | jq_ "[a['message'] for a in d[:10]]"
  grep -E "git_push|GitPush|previews" "$H/rolld.log" | tail -12 | cut -c1-260
  exit 1
}
T7=$(now_ms)
echo "site feature 1" > "$WT/index.html"
gitc commit -qam "feature 1"
gitc push -q origin HEAD:refs/heads/feature
SAID=$(hook_push "$(gitc rev-parse HEAD)" "feature 1")
case "$SAID" in *"handed to roll previews"*) ;; *) echo "the push should be handed to roll previews: $SAID"; exit 1;; esac
serves "site feature 1"
T8=$(now_ms)
GREPO=$(api GET "/v1/previews/$GPV" | jq_ "d['repo']")
[ "$GREPO" = "git:$SVC2" ] || { echo "the Git URL service's preview should be of git:$SVC2, not $GREPO"; exit 1; }
FIRST=$GPV
echo "site feature 2" > "$WT/index.html"
gitc commit -qam "feature 2"
gitc push -q origin HEAD:refs/heads/feature
hook_push "$(gitc rev-parse HEAD)" "feature 2" >/dev/null
hook_push "$(gitc rev-parse HEAD)" "feature 2" >/dev/null
serves "site feature 2"
[ "$GPV" = "$FIRST" ] || { echo "the next push should move the branch's preview, not make $GPV"; exit 1; }
GCOUNT=$(git_preview | python3 -c "import sys,ast; print(len(ast.literal_eval(sys.stdin.read())))")
[ "$GCOUNT" = 1 ] || { echo "the same push twice should keep one preview: $(git_preview)"; exit 1; }
gitc push -q origin :refs/heads/feature
hook_push 0000000000000000000000000000000000000000 "" >/dev/null
for _ in $(seq 1 240); do [ "$(git_preview)" = "[]" ] && break; sleep 0.5; done
[ "$(git_preview)" = "[]" ] || { echo "deleting the branch should delete its preview: $(git_preview)"; exit 1; }
GIT_SAID="made and serving in $((T8 - T7)) ms; moved by the next push (the same preview); deleted with the branch"

say "a pull request of the Git URL service: a same-repo one names its branch's preview; a fork's runs only once a writer approves its head; a close deletes it"
pr_event() { # pr_event <action> <number> <head branch> <sha> <head repo id> [hook]: GitHub's pull_request event
  curl -sS -X POST "http://$API${6:-$HOOK}" -H 'content-type: application/json' -H 'x-github-event: pull_request' \
    -d "$(python3 -c 'import json,sys; a,n,b,s,r=sys.argv[1:]; print(json.dumps({"action": a, "number": int(n), "pull_request": {"number": int(n), "title": "Cart badge (#" + n + ")", "head": {"ref": b, "sha": s, "repo": {"id": int(r)}}, "base": {"repo": {"id": 1}}}}))' "$1" "$2" "$3" "$4" "$5")"
}
pull_preview() { api GET "/v1/previews?project=$PRJ" | jq_ "([p for p in d if p['branch'] == '$1'] + [None])[0]"; }
gitc checkout -q -B cart origin/main 2>/dev/null || gitc checkout -q -B cart main
echo "site cart" > "$WT/index.html"
gitc commit -qam "cart"
gitc push -q origin HEAD:refs/heads/cart
CART=$(gitc rev-parse HEAD)
SAID=$(pr_event opened 5 cart "$CART" 1)
case "$SAID" in *"handed to roll previews"*) ;; *) echo "a same-repo pull request should be handed to roll previews: $SAID"; exit 1;; esac
PR_SAME=""
for _ in $(seq 1 600); do
  PR_SAME=$(pull_preview cart | python3 -c "import sys,ast; p=ast.literal_eval(sys.stdin.read()); print('' if not p else '%s|%s|%s' % (p['title'], (p.get('pull') or {}).get('number'), p['trusted']))")
  case "$PR_SAME" in "Cart badge (#5)|5|True") break;; esac
  sleep 1
done
[ "$PR_SAME" = "Cart badge (#5)|5|True" ] || { echo "the same-repo pull request should name its branch's preview, trusted: $PR_SAME"; exit 1; }
# A fork's: its head is the base repository's refs/pull/7/head (GitHub keeps it there). roll
# code's git takes no such ref, so the fork's repository is served by git's own smart HTTP
# (git http-backend behind a few lines of Python), refs/pull/7/head written by a file push.
GIT_ROOT="$H/gitroot"
mkdir -p "$GIT_ROOT/acme" && git init -q --bare "$GIT_ROOT/acme/shop.git"
gitc push -q "file://$GIT_ROOT/acme/shop.git" HEAD:refs/heads/main HEAD:refs/pull/7/head
python3 - "$GIT_ROOT" "$GIT_PORT" >"$H/githttp.log" 2>&1 <<'PY' &
import http.server, os, subprocess, sys
root, port = sys.argv[1], int(sys.argv[2])
class H(http.server.BaseHTTPRequestHandler):
    def run(self):
        path, _, qs = self.path.partition("?")
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n) if n else b""
        env = dict(os.environ, GIT_PROJECT_ROOT=root, GIT_HTTP_EXPORT_ALL="1", PATH_INFO=path, QUERY_STRING=qs, REQUEST_METHOD=self.command,
                   CONTENT_TYPE=self.headers.get("content-type", ""), CONTENT_LENGTH=str(len(body)), GIT_PROTOCOL=self.headers.get("git-protocol", ""),
                   HTTP_CONTENT_ENCODING=self.headers.get("content-encoding", ""))
        out = subprocess.run(["git", "http-backend"], input=body, env=env, capture_output=True).stdout
        head, _, rest = out.partition(b"\r\n\r\n")
        status, hdrs = 200, []
        for line in head.decode().split("\r\n"):
            k, _, v = line.partition(":")
            if k.lower() == "status":
                status = int(v.split()[0])
            elif k:
                hdrs.append((k, v.strip()))
        self.send_response(status)
        for k, v in hdrs:
            self.send_header(k, v)
        self.send_header("content-length", str(len(rest)))
        self.end_headers()
        self.wfile.write(rest)
    do_GET = do_POST = run
    def log_message(self, *a):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY
GITD=$!
for _ in $(seq 1 40); do curl -sf -o /dev/null "http://127.0.0.1:$GIT_PORT/acme/shop.git/info/refs?service=git-upload-pack" && break; sleep 0.25; done
SVC3=$(api POST "/v1/apps/projects/$PRJ/services" "{\"name\":\"shopfront\",\"source\":{\"kind\":\"git\",\"url\":\"http://127.0.0.1:$GIT_PORT/acme/shop.git\"},\"run\":{\"type\":\"web\",\"port\":8000}}" | jq_ "d['id']")
# On roll's GitHub App (the operator's call): its previews report where its code lives.
[ "$(api PUT "/v1/apps/services/$SVC3/github" | jq_ "(d.get('github') or {}).get('repo')")" = acme/shop ] || { echo "the Git URL service should connect to roll's GitHub App: $(api PUT "/v1/apps/services/$SVC3/github")"; exit 1; }
gh_log() { curl -sS "http://127.0.0.1:$GH_PORT/_fake/log"; }
HOOK3=$(api POST "/v1/apps/services/$SVC3/environments/production/hook" | jq_ "d['path']")
SAID=$(pr_event opened 7 cart "$CART" 2 "$HOOK3")
case "$SAID" in *"handed to roll previews"*) ;; *) echo "a fork's pull request should be handed to roll previews: $SAID"; exit 1;; esac
FORK=""
for _ in $(seq 1 120); do
  FORK=$(api GET "/v1/previews?project=$PRJ" | jq_ "([p['id'] + '|' + p['state'] + '|' + str(p['trusted']) for p in d if p['branch'] == 'pull/7' and p['repo'] == 'git:$SVC3'] + [''])[0]")
  case "$FORK" in *"|awaiting_approval|False") break;; esac
  sleep 1
done
case "$FORK" in *"|awaiting_approval|False") ;; *) echo "a fork's pull request should wait for an approval, untrusted: $FORK"; exit 1;; esac
FORK_ID=${FORK%%|*}
# Said on GitHub while it waits: roll/preview pending on its head, with no link to what hasn't run.
for _ in $(seq 1 60); do gh_log | grep -q '"roll/preview"' && break; sleep 0.5; done
[ "$(gh_log | jq_ "[s['state'] for s in d if s['kind'] == 'status' and s['context'] == 'roll/preview' and s['sha'] == '$CART'][-1:]")" = "['pending']" ] || { echo "a fork's waiting preview should be roll/preview pending on its head: $(gh_log)"; exit 1; }
[ "$(api GET "/v1/previews/$FORK_ID" | jq_ "any(s.get('deployed') for s in d['services'])")" = False ] || { echo "nothing of a fork's should be deployed before an approval: $(api GET "/v1/previews/$FORK_ID" | jq_ "[(s['name'], s.get('deployed')) for s in d['services']]")"; exit 1; }
STALE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://$API/v1/previews/$FORK_ID/approve" -H 'content-type: application/json' -d '{"head":"0000000000000000000000000000000000000000"}')
[ "$STALE" = 409 ] || { echo "approving another head should be refused: $STALE"; exit 1; }
# A fork's preview reaches roll only by default (egress_forks): roll apps runs it closed, its
# build without its secrets (build_closed, apps' #222).
api POST "/v1/previews/$FORK_ID/approve" "{\"head\":\"$CART\"}" >/dev/null
for _ in $(seq 1 600); do
  [ "$(api GET "/v1/previews/$FORK_ID" | jq_ "d['state']")" = ready ] && break
  sleep 1
done
[ "$(api GET "/v1/previews/$FORK_ID" | jq_ "d['state']")" = ready ] || { echo "the approved head should run: $(api GET "/v1/previews/$FORK_ID" | jq_ "{k: d.get(k) for k in ('state', 'error')}")"; exit 1; }
# Ready: roll/preview success, and the pull request's comment, made once and edited after.
for _ in $(seq 1 120); do
  [ "$(gh_log | jq_ "[s['state'] for s in d if s['kind'] == 'status' and s['context'] == 'roll/preview' and s['sha'] == '$CART'][-1:]")" = "['success']" ] && gh_log | grep -q '"number": 7' && break
  sleep 0.5
done
GH_SAID=$(gh_log | jq_ "'%s; %d comment(s) on #7, %d edit(s)' % ('/'.join(s['state'] for s in d if s['kind'] == 'status' and s['context'] == 'roll/preview' and s['sha'] == '$CART'), sum(1 for c in d if c['kind'] == 'comment' and c['number'] == 7), sum(1 for c in d if c['kind'] == 'edit'))")
case "$GH_SAID" in *success\;*" 1 comment(s) on #7"*) ;; *) echo "the approved fork's preview should be roll/preview success with one comment on #7: $GH_SAID"; gh_log; exit 1;; esac
pr_event closed 7 cart "$CART" 2 "$HOOK3" >/dev/null
for _ in $(seq 1 120); do [ -z "$(api GET "/v1/previews?project=$PRJ" | jq_ "''.join(p['id'] for p in d if p['branch'] == 'pull/7')")" ] && break; sleep 0.5; done
[ -z "$(api GET "/v1/previews?project=$PRJ" | jq_ "''.join(p['id'] for p in d if p['branch'] == 'pull/7')")" ] || { echo "closing the fork's pull request should delete its preview"; exit 1; }
PR_FORK="waited for an approval with nothing deployed (another head refused, 409), ran once approved, deleted when closed"
pr_event closed 5 cart "$CART" 1 >/dev/null
for _ in $(seq 1 120); do [ "$(pull_preview cart)" = None ] && break; sleep 0.5; done
[ "$(pull_preview cart)" = None ] || { echo "closing the pull request should delete its preview"; exit 1; }
cat <<EOF
| What | Measured |
| --- | --- |
| Change opened → preview ready (Redis branched, roll apps environment made, deploy queued) | $((T1 - T0)) ms |
| The roll apps service's screenshot (its instance's network) / load / against main's baseline on roll apps / its [app.previews.screenshots] states | $SHOT / $LOAD ms / $BASE / $APPS_STATE |
| The feedback button on the roll apps environment (a visitor through roll) / its notes | tagged / $NOTES |
| … → its deploy active (a Docker build of the change's commit) | $((T2 - T0)) ms |
| First request through roll apps (cold) | $((T4 - T3)) ms |
| X-Robots-Tag on the pv- environment's page | $APPS_ROBOTS |
| Roll only on the pv- instance (a direct connection / a listed host / an unlisted one) | $EGRESS_APPS |
| Body / the Redis host the service saw / the preview's Redis branch | $BODY / $WHERE / $BRANCH |
| Variables on the environment | $VARS |
| A stream link's KAFKA_USERNAME on the pv- environment | the branch's SASL user ($KAFKA_PV), not production's link's |
| A preview's own variable on roll apps | $UNSET |
| Promoted to production (the preview's image, no build): production serves / took / expecting another deployment | $PROD_AFTER / $((T6 - T5)) ms / HTTP $STALE |
| After landing: previews left / the environment | $N / HTTP $GONE |
| A Git URL service's branch (a git push and its webhook): its preview | $GIT_SAID |
| A Git URL service's pull requests: same repo / a fork's | named and trusted, deleted when closed / $PR_FORK |
| … on GitHub, through roll's GitHub App (a fake): the fork's roll/preview / its comment | $GH_SAID |
EOF
