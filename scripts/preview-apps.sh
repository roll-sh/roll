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
PRJ=""
cleanup() {
  [ -n "${PID:-}" ] && { kill "$PID" 2>/dev/null || true; for _ in $(seq 1 20); do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done; kill -9 "$PID" 2>/dev/null || true; }
  for c in $(docker ps -aq --filter "label=sh.roll.code.home=$H"); do docker rm -f "$c" >/dev/null; done
  if [ -n "$SVC" ]; then
    s=${SVC#svc_}
    for c in $(docker ps -aq --filter "name=^roll-apps-$s-"); do docker rm -f "$c" >/dev/null; done
    for i in $(docker images --format '{{.Repository}}:{{.Tag}}' | grep "^roll-apps-$s:" || true); do docker rmi -f "$i" >/dev/null || true; done
    for v in $(docker volume ls -q | grep "^roll-apps-vol-$s-" || true); do docker volume rm -f "$v" >/dev/null || true; done
  fi
  if [ -n "$PRJ" ]; then
    for n in $(docker network ls --format '{{.Name}}' | grep "^roll-apps-net-${PRJ#prj_}-" || true); do docker network rm "$n" >/dev/null || true; done
  fi
}
trap cleanup EXIT
if curl -s -o /dev/null --max-time 1 "http://$API/v1/info"; then echo "something answers on $API" >&2; exit 1; fi
rm -rf "$H" && mkdir -p "$H"
export ROLL_HOME=$H ROLL_API=$API ROLL_PROXY=127.0.0.1:${PORTS}432 ROLL_REDIS=127.0.0.1:${PORTS}380 ROLL_REDIS_HTTP=127.0.0.1:${PORTS}081 \
  ROLL_APPS_LISTEN=127.0.0.1:${PORTS}080 ROLL_APPS_DOMAIN=apps.localhost ROLL_CODE_PREVIEW_DOMAIN=preview.localhost \
  ROLL_PRODUCTS=code,redis,apps RUST_LOG=${RUST_LOG:-info}
unset ROLL_ADMIN_TOKEN ROLL_CODE_TOKEN
nice -n 10 "$BIN" >"$H/rolld.log" 2>&1 &
PID=$!
for _ in $(seq 1 120); do kill -0 "$PID" 2>/dev/null || { tail -20 "$H/rolld.log"; exit 1; }; curl -sf "http://$API/v1/info" >/dev/null 2>&1 && break; sleep 0.5; done

say "a repo with a Dockerfile, a Redis database, a project, a roll apps service"
api POST /v1/code/owners '{"name":"acme","kind":"org"}' >/dev/null
api POST /v1/code/repos '{"owner":"acme","name":"web","source":"native"}' >/dev/null
BODY_INIT=$(python3 - <<'PY'
import json
start = "#!/bin/sh\necho \"$REDIS_URL\" | sed -E 's#.*default:(rdb_[a-z0-9]+)_.*#\\1#' > where\nexec python -m http.server ${PORT:-8000}\n"
docker = 'FROM python:3.13-alpine\nWORKDIR /app\nCOPY . .\nCMD ["sh", "start.sh"]\n'
print(json.dumps({"message": "init", "files": {"Dockerfile": {"content": docker}, "start.sh": {"content": start}, "index.html": {"content": "main"}}}))
PY
)
api POST /v1/code/repos/acme/web/land "$BODY_INIT" >/dev/null
REDIS=$(api POST /v1/redis/databases '{"name":"cache"}' | jq_ "d['id']")
PRJ=$(api POST /v1/workspace/projects '{"name":"shop"}' | jq_ "d['id']")
for r in "repo/acme%2Fweb" "redis/$REDIS"; do api PUT "/v1/workspace/projects/$PRJ/resources/$r" '{"x":0,"y":0}' >/dev/null; done
SVCJ=$(api POST "/v1/apps/projects/$PRJ/services" '{"name":"web","source":{"kind":"repo","repo":"acme/web"},"run":{"type":"web","port":8000}}')
SVC=$(echo "$SVCJ" | jq_ "d['id']")
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
HOSTPART=$(echo "$URL" | sed -E 's#^https?://##; s#/.*##; s#:.*##')
T3=$(now_ms)
BODY=$(curl -sS -H "Host: $HOSTPART" "http://127.0.0.1:${PORTS}080/" --max-time 120)
WHERE=$(curl -sS -H "Host: $HOSTPART" "http://127.0.0.1:${PORTS}080/where" --max-time 30)
T4=$(now_ms)
[ "$BODY" = "preview of change 1" ] || { echo "the preview should serve the change's page: $BODY"; exit 1; }

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

say "a screenshot of the roll apps service (in its instance's network), and main's baseline on roll apps"
api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"screenshot":true}}' >/dev/null
for _ in $(seq 1 240); do
  [ "$(api GET "/v1/previews/$PV" | jq_ "d.get('screenshot_at') is not None")" = True ] && break
  sleep 1
done
SHOT=$(curl -sS "http://$API/v1/previews/$PV/screenshot" | python3 -c "import sys; b=sys.stdin.buffer.read(); print(f'{len(b)} bytes, PNG' if b[:4]==b'\x89PNG' else 'not a PNG: ' + b[:200].decode('utf-8','replace'))")
case "$SHOT" in *PNG) ;; *) echo "the roll apps service should be screenshotted: $SHOT"; api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d[:8]]"; exit 1;; esac
LOAD=$(api GET "/v1/previews/$PV" | jq_ "(d.get('page_speed') or {}).get('now', {}).get('load_ms')")
BASE=""
for _ in $(seq 1 600); do
  BASE=$(api GET "/v1/previews/$PV" | jq_ "('%s%% against %s' % (d['base_change']['percent'], d['base_change']['branch'])) if d.get('base_change') else ''")
  [ -n "$BASE" ] && break
  sleep 1
done
[ -n "$BASE" ] || { echo "main's baseline on roll apps should be compared with the change"; api GET "/v1/previews?project=$PRJ" | jq_ "[(p['branch'], p['state'], p.get('screenshot_at')) for p in d]"; exit 1; }
api PUT "/v1/previews/projects/$PRJ/settings" '{"github":{"screenshot":false}}' >/dev/null
BRANCH=$(echo "$P" | jq_ "[r['ref'] for r in d['resources'] if r['kind']=='redis'][0]")
say "land"
api POST "/v1/code/changes/$CHG/land" '{}' >/dev/null
for _ in $(seq 1 120); do N=$(api GET "/v1/previews?project=$PRJ" | jq_ "len(d)"); [ "$N" = 0 ] && break; sleep 0.5; done
GONE=$(curl -s -o /dev/null -w '%{http_code}' "http://$API/v1/apps/services/$SVC/environments/$ENV")
cat <<EOF
| What | Measured |
| --- | --- |
| Change opened → preview ready (Redis branched, roll apps environment made, deploy queued) | $((T1 - T0)) ms |
| The roll apps service's screenshot (its instance's network) / load / against main's baseline on roll apps | $SHOT / $LOAD ms / $BASE |
| … → its deploy active (a Docker build of the change's commit) | $((T2 - T0)) ms |
| First request through roll apps (cold) | $((T4 - T3)) ms |
| Body / the Redis host the service saw / the preview's Redis branch | $BODY / $WHERE / $BRANCH |
| Variables on the environment | $VARS |
| A preview's own variable on roll apps | $UNSET |
| After landing: previews left / the environment | $N / HTTP $GONE |
EOF
