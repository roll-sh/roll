#!/usr/bin/env bash
# The MCP sweep: build rolld, start it with every product on, all its listeners on free loopback
# ports and a throwaway home, and call every MCP tool through /v1/mcp as an agent would
# (scripts/mcp-sweep.py: reads, then writes). It finds what the unit tests can't: a tool whose
# path, query or body the product's own handler rejects, an id the product routes on differently,
# a 5xx. Run it after adding or changing MCP tools, or a product's routes or owner routing.
#
# Only runs when something it covers changed since the last green run. `--force` runs regardless;
# `--reads` skips the write tools. Needs nothing outside this machine (Docker isn't used: products
# whose tools need it answer refusals the sweep counts as fine).
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates Cargo.toml Cargo.lock scripts/test-mcp-sweep.sh scripts/mcp-sweep.py)
STAMP=target/.test-mcp-sweep.hash

force=0 writes=--writes
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --reads) writes= ;;
    *) echo "unknown argument $a (--force, --reads)" >&2; exit 2 ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "mcp sweep: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

cargo build -q -p rolld
bin=${CARGO_TARGET_DIR:-target}/debug/rolld

# 25 single ports and a range of 21 for apps' per-environment ports, all free on loopback now.
read -r -a ports < <(python3 -I - <<'EOF'
import random, socket
def free(p):
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", p)); return True
    except OSError:
        return False
    finally:
        s.close()
for _ in range(200):
    base = random.randrange(20000, 30000)
    if all(free(p) for p in range(base, base + 46)):
        print(" ".join(str(p) for p in range(base, base + 46))); break
EOF
)
[[ ${#ports[@]} == 46 ]] || { echo "mcp sweep: no 46 free loopback ports in a row" >&2; exit 1; }

home=$(mktemp -d)
pid=
cleanup() {
  [[ -n $pid ]] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  rm -rf "$home"
}
trap cleanup EXIT
umask 077
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$home/token"
export ROLL_ADMIN_TOKEN=$(cat "$home/token") ROLL_SECRET_KEY=$(head -c 32 /dev/urandom | base64)
L=127.0.0.1 p=("${ports[@]}")
flags=(--home "$home/rolld" --api $L:${p[0]} --proxy $L:${p[1]} --redis $L:${p[2]} --redis-http $L:${p[3]}
  --kafka $L:${p[4]} --s3 $L:${p[5]} --storage-api $L:${p[6]} --queues-amqp $L:${p[7]} --queues-stomp $L:${p[8]}
  --queues-mqtt $L:${p[9]} --queues-stream $L:${p[10]} --nosql-cql $L:${p[11]} --queues-management $L:${p[12]}
  --queues-web-stomp $L:${p[13]} --queues-web-mqtt $L:${p[14]} --queues-http $L:${p[15]}
  --nosql-cql-shard-aware $L:${p[16]} --nosql-alternator $L:${p[17]} --nosql-rest $L:${p[18]}
  --previews-smtp $L:${p[19]} --apps-listen $L:${p[20]} --search-http $L:${p[21]}
  --apps-ports "${p[25]}-${p[45]}" --apps-ports-host $L)
"$bin" "${flags[@]}" > "$home/rolld.log" 2>&1 &
pid=$!
api=http://$L:${p[0]}
for _ in $(seq 120); do
  [[ "$(curl -s -m 3 -o /dev/null -w '%{http_code}' "$api/v1/access")" == 401 ]] && break
  kill -0 "$pid" 2>/dev/null || { echo "mcp sweep: rolld exited:" >&2; tail -20 "$home/rolld.log" >&2; exit 1; }
  sleep 1
done

if python3 -I scripts/mcp-sweep.py "$api" "$home/token" $writes; then
  mkdir -p target && echo "$hash" > "$STAMP"
else
  mkdir -p target && cp "$home/rolld.log" target/mcp-sweep-rolld.log
  echo "mcp sweep: failures above (rolld's log kept in target/mcp-sweep-rolld.log)" >&2
  exit 1
fi
