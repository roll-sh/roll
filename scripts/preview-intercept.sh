#!/usr/bin/env bash
# Keys a preview never holds, end to end (docs/previews.md, "Keys a preview never holds"): a key in
# roll secrets bound to a host, a preview whose egress goes through its proxy ("Roll only") and
# whose rules a token saved (the placeholders' maker), and a TLS host of our own that answers
# with the Authorization header it got. Checks that the preview's service holds a placeholder,
# never the key; that a request through the preview's interception socket (as its egress proxy
# hands one over) reaches the host with the key swapped in; that a placeholder that isn't live is
# refused there; and that the activity says so.
#
# The host is local (`echo.localhost`), so rolld runs with ROLL_PREVIEWS_INTERCEPT_ALLOW_PRIVATE
# (local development only) trusting its CA. The egress proxy only lets ports 443 and 80 through,
# and 443 isn't ours here, so the request goes to the socket directly: what egress.js would send.
#
#   scripts/preview-intercept.sh [--keep]
set -euo pipefail
P=${PREVIEW_INTERCEPT_PORTS:-23}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${ROLLD:-$ROOT/target/debug/rolld}
HOME_DIR=${PREVIEW_INTERCEPT_HOME:-$HOME/.cache/roll-previews-intercept-e2e}
KEEP=${1:-}
API=127.0.0.1:${P}070
ECHO_PORT=${P}443
IMAGE=${PREVIEW_IMAGE:-python:3.13-alpine}
KEY_VALUE=sk_live_e2e_9f2c41
say() { printf '\033[2m%s\033[0m\n' "$*" >&2; }
api() { # api METHOD PATH [JSON]
  local m=$1 p=$2 b=${3:-}
  if [ -n "$b" ]; then curl -sS -X "$m" "http://$API$p" -H 'content-type: application/json' -d "$b"; else curl -sS -X "$m" "http://$API$p"; fi
}
jq_() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {'d': d}))" "$1"; }

cleanup() {
  [ -n "${ECHO_PID:-}" ] && kill "$ECHO_PID" 2>/dev/null || true
  if [ "$KEEP" != "--keep" ] && [ -n "${PID:-}" ]; then
    kill "$PID" 2>/dev/null || true
    for _ in $(seq 1 20); do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done
    kill -9 "$PID" 2>/dev/null || true
    for c in $(docker ps -aq --filter "label=sh.roll.code.home=$HOME_DIR"); do docker rm -f "$c" >/dev/null 2>&1 || true; done
    for n in $(docker network ls -q --filter "label=sh.roll.code.home=$HOME_DIR"); do docker network rm "$n" >/dev/null 2>&1 || true; done
  fi
}
trap cleanup EXIT

if curl -s -o /dev/null --max-time 1 "http://$API/v1/info"; then echo "something already answers on $API: stop it first" >&2; exit 1; fi
rm -rf "$HOME_DIR" && mkdir -p "$HOME_DIR"

say "the host a key is bound to: echo.localhost, TLS with a CA of its own, on [::1]:$ECHO_PORT"
cd "$HOME_DIR"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout ca.key -out ca.pem -days 1 -subj "/CN=roll e2e echo CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout echo.key -out echo.csr -subj "/CN=echo.localhost" 2>/dev/null
openssl x509 -req -in echo.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out echo.pem -days 1 -extfile <(printf "subjectAltName=DNS:echo.localhost") 2>/dev/null
python3 - "$ECHO_PORT" <<'PY' &
import hashlib, http.server, socket, ssl, sys
class Six(http.server.ThreadingHTTPServer):
    address_family = socket.AF_INET6
class Echo(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        # `/`: a hash of what it got (proves the swap without bringing the key back); `/echo`: the
        # header itself, which roll secrets' scrub must stop.
        auth = self.headers.get("Authorization") or ""
        body = ("authorization=" + auth if self.path == "/echo" else "sha256=" + hashlib.sha256(auth.encode()).hexdigest()).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a):
        pass
s = Six(("::1", int(sys.argv[1])), Echo)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain("echo.pem", "echo.key")
s.socket = ctx.wrap_socket(s.socket, server_side=True)
s.serve_forever()
PY
ECHO_PID=$!
cd "$ROOT"

export ROLL_HOME=$HOME_DIR ROLL_API=$API ROLL_PROXY=127.0.0.1:${P}432 ROLL_CODE_PREVIEW_DOMAIN=preview.localhost ROLL_PRODUCTS=code RUST_LOG=${RUST_LOG:-info}
unset ROLL_ADMIN_TOKEN ROLL_CODE_TOKEN
export ROLL_SECRET_KEY=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
export ROLL_PREVIEWS_INTERCEPT_ALLOW_PRIVATE=$HOME_DIR/ca.pem
say "starting rolld in $HOME_DIR"
nice -n 10 "$BIN" >"$HOME_DIR/rolld.log" 2>&1 &
PID=$!
for _ in $(seq 1 120); do
  kill -0 "$PID" 2>/dev/null || { tail -20 "$HOME_DIR/rolld.log"; exit 1; }
  curl -sf "http://$API/v1/info" >/dev/null 2>&1 && break
  sleep 0.5
done
grep -q "ROLL_PREVIEWS_INTERCEPT_ALLOW_PRIVATE is set" "$HOME_DIR/rolld.log" || { echo "rolld should warn that the local-only override is on"; exit 1; }

say "a repo, a project, a key in roll secrets bound to echo.localhost"
api POST /v1/code/owners '{"name":"acme","kind":"org"}' >/dev/null
api POST /v1/code/repos '{"owner":"acme","name":"web","source":"native"}' >/dev/null
TOML='[preview]\nimage = \"'"$IMAGE"'\"\nrun = \"env > env.txt && python -m http.server $PORT\"\nport = 8000\nhealth = \"/\"\nmemory = 128\ncpus = 0.25\n'
api POST /v1/code/repos/acme/web/land "{\"message\":\"init\",\"files\":{\"roll.toml\":{\"content\":\"$TOML\"},\"index.html\":{\"content\":\"main\"}}}" >/dev/null
PRJ=$(api POST /v1/workspace/projects '{"name":"shop"}' | jq_ "d['id']")
api PUT "/v1/workspace/projects/$PRJ/resources/repo/acme%2Fweb" '{"x":0,"y":0}' >/dev/null
SET=$(curl -s -o /dev/null -w '%{http_code}' -X PUT "http://$API/v1/secrets/projects/$PRJ/environments/development/secrets/STRIPE_KEY" -H 'content-type: application/json' -d "{\"value\":\"$KEY_VALUE\"}")
case "$SET" in 200|201) ;; *) echo "setting STRIPE_KEY: HTTP $SET"; exit 1;; esac
BOUND=$(curl -s -o "$HOME_DIR/bind.json" -w '%{http_code}' -X PUT "http://$API/v1/secrets/projects/$PRJ/environments/development/secrets/STRIPE_KEY/hosts" -H 'content-type: application/json' -d '{"hosts":["echo.localhost"],"header":"Authorization: Bearer {}"}')
case "$BOUND" in 200|201|204) ;; *) echo "binding STRIPE_KEY to echo.localhost: HTTP $BOUND $(cat "$HOME_DIR/bind.json")"; exit 1;; esac

say "a token saves the rules (the placeholders' maker): Roll only, echo.localhost allowed, public"
TOKEN=$(api POST /v1/access/tokens "{\"name\":\"rules\",\"rules\":[{\"product\":\"previews\",\"resources\":\"*\",\"actions\":{\"read\":\"allow\",\"write\":\"allow\",\"admin\":\"allow\"}},{\"product\":\"secrets\",\"resources\":[{\"project\":\"$PRJ\"}],\"actions\":{\"read\":\"allow\",\"reveal\":\"allow\"}}]}" | jq_ "d['token']")
SAVED=$(curl -s -o "$HOME_DIR/rules.json" -w '%{http_code}' -X PUT "http://$API/v1/previews/projects/$PRJ/settings" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"enabled":true,"egress":"roll","egress_hosts":["echo.localhost"],"access":"public"}')
[ "$SAVED" = 200 ] || { echo "the token saving the rules: HTTP $SAVED $(cat "$HOME_DIR/rules.json")"; exit 1; }

say "open a change"
api POST /v1/code/repos/acme/web/changes '{"title":"Pay","message":"Pay","base":"main","files":{"index.html":{"content":"change"}}}' >/dev/null
PV=""
for _ in $(seq 1 240); do
  PV=$(api GET "/v1/previews?project=$PRJ" | jq_ "d[0]['id'] if d and d[0]['state'] in ('ready','sleeping') else ''")
  [ -n "$PV" ] && break
  sleep 0.5
done
[ -n "$PV" ] || { api GET "/v1/previews/$(api GET "/v1/previews?project=$PRJ" | jq_ "d[0]['id']")/activity" | head -c 2000; tail -30 "$HOME_DIR/rolld.log"; exit 1; }
HOST=$(api GET "/v1/previews/$PV" | jq_ "[x for x in d['services'] if x['name'] == 'web'][0]['url'].split('//')[1].split(':')[0]")
ENVTXT=$(curl -sS -H "Host: $HOST" "http://$API/env.txt")
PLACEHOLDER=$(echo "$ENVTXT" | sed -n 's/^STRIPE_KEY=//p')
case "$PLACEHOLDER" in rlph_*) ;; *) echo "the preview's service should hold a placeholder for STRIPE_KEY, not '${PLACEHOLDER:0:12}…'"; exit 1;; esac
echo "$ENVTXT" | grep -q "$KEY_VALUE" && { echo "the key's value reached the preview's service"; exit 1; }
echo "$ENVTXT" | grep -q '^ROLL_INTERCEPT_CA=' || { echo "the service should be given its interception CA"; exit 1; }

say "through the preview's interception socket: the key swapped in, a placeholder that isn't live refused"
SOCK=$HOME_DIR/previews/intercept/$PV/sock/intercept.sock
CA=$HOME_DIR/previews/intercept/$PV/ca.pem
[ -S "$SOCK" ] && [ -f "$CA" ] || { echo "the preview's interception socket and CA should be there: $SOCK"; exit 1; }
through() { # through <authorization> [path]: the status and body the host answered, through the socket ("cut" when none came)
  python3 - "$SOCK" "$CA" "$ECHO_PORT" "$1" "${2:-/}" <<'PY'
import socket, ssl, sys
sock, ca, port, auth, path = sys.argv[1:]
s = socket.socket(socket.AF_UNIX); s.connect(sock)
s.sendall(f"echo.localhost:{port}\n".encode())
ctx = ssl.create_default_context(cafile=ca)
t = ctx.wrap_socket(s, server_hostname="echo.localhost")
t.sendall(f"GET {path} HTTP/1.1\r\nHost: echo.localhost\r\nAuthorization: {auth}\r\nConnection: close\r\n\r\n".encode())
out = b""
try:
    while True:
        b = t.recv(65536)
        if not b:
            break
        out += b
except (ssl.SSLError, OSError):
    pass
head, sep, body = out.partition(b"\r\n\r\n")
print(head.split(b" ")[1].decode() + " " + body.decode(errors="replace") if sep else "cut " + out.decode(errors="replace"))
PY
}
GOT=$(through "Bearer $PLACEHOLDER")
WANT=$(printf 'Bearer %s' "$KEY_VALUE" | sha256sum | cut -d' ' -f1)
[ "$GOT" = "200 sha256=$WANT" ] || { echo "the host should get the key swapped in (its hash back), not: ${GOT:0:120}"; exit 1; }
WRONG=$(through "Bearer rlph_$(python3 -c 'print("a"*40)')")
case "$WRONG" in 403*) ;; *) echo "a placeholder that isn't live should be refused (403), not: ${WRONG:0:120}"; exit 1;; esac
ACT=$(api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d]")
echo "$ACT" | grep -q "egress to echo.localhost: 1 request(s), 1 key(s) swapped in" || { echo "the activity should say a key was swapped in: $ACT"; exit 1; }
echo "$ACT" | grep -q "$KEY_VALUE" && { echo "the activity should never show the key"; exit 1; }

say "a host that echoes the key back: roll secrets' scrub stops it, and the placeholder ends"
ECHOED=$(through "Bearer $PLACEHOLDER" /echo)
case "$ECHOED" in *"$KEY_VALUE"*) echo "an echoed key reached the preview's service"; exit 1;; esac
case "$ECHOED" in 502*|cut*|"200 ") ;; *) echo "an echo should be stopped (a 502, or the connection cut), not: ${ECHOED:0:120}"; exit 1;; esac
AFTER=""
for _ in $(seq 1 20); do
  AFTER=$(through "Bearer $PLACEHOLDER")
  case "$AFTER" in 403*) break;; esac
  sleep 0.5
done
case "$AFTER" in 403*) ;; *) echo "the echoed placeholder should be ended (403 after), not: ${AFTER:0:120}"; exit 1;; esac
ACT=$(api GET "/v1/previews/$PV/activity" | jq_ "[a['message'] for a in d]")
echo "$ACT" | grep -q "echoed STRIPE_KEY" || { echo "the activity should say the host echoed STRIPE_KEY: $ACT"; exit 1; }
echo "$ACT" | grep -q "$KEY_VALUE" && { echo "the activity should never show the key"; exit 1; }
say "ok: the service held $(echo "$PLACEHOLDER" | cut -c1-9)…, the host got the key, a dead placeholder was refused, an echo was stopped and ended its placeholder"
