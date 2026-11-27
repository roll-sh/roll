#!/usr/bin/env bash
# roll's MCP server against the official MCP conformance suite (@modelcontextprotocol/conformance,
# server scenarios), every revision roll speaks and the 2026-07-28 draft scenarios.
#
# Builds rolld and starts it open (no ROLL_ADMIN_TOKEN, every listener on loopback port 0), so the
# suite connects as the operator, then compares the outcome with tests/mcp-conformance/expected.yml:
# every check known not to pass and why (most call the fixture tools, prompts and resources the
# suite's reference server has, which a real server doesn't). A check failing that isn't listed is
# a regression, and a listed one passing is stale: remove its line. Every message roll sends is
# also checked against the spec's JSON schema for its revision (`wire-schema-valid`).
#
# Only runs when something it covers changed since the last green run; `--force` runs regardless.
# The results are in target/mcp-conformance/ (`checks.json` per scenario, rolld's log).
set -euo pipefail
cd "$(dirname "$0")/.."

# The suite, pinned. Bump it on purpose and update expected.yml in the same change.
SUITE=@modelcontextprotocol/conformance@0.2.0-alpha.12
PATHS=(crates Cargo.toml Cargo.lock tests/mcp-conformance scripts/test-mcp-conformance.sh)
STAMP=target/.test-mcp-conformance.hash

force=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    *) echo "unknown argument $a (--force)" >&2; exit 2 ;;
  esac
done
hash=$( (echo "$SUITE"; find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256) | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "mcp conformance: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

cargo build -q -p rolld
bin=${CARGO_TARGET_DIR:-target}/debug/rolld
out=target/mcp-conformance
rm -rf "$out" && mkdir -p "$out"

home=$(mktemp -d)
pid=
dock() { if docker info >/dev/null 2>&1; then docker "$@"; else sg docker -c "docker $(printf '%q ' "$@")"; fi; }
cleanup() {
  [[ -n $pid ]] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  # The computes this rolld started: exactly the containers that mount its home.
  if command -v docker >/dev/null 2>&1; then
    for n in $(dock ps -a --format '{{.Names}}' 2>/dev/null | grep '^roll-'); do
      dock inspect -f '{{range .Mounts}}{{.Source}} {{end}}' "$n" 2>/dev/null | grep -q "$home/" && dock rm -f -v "$n" >/dev/null 2>&1 || true
    done
  fi
  rm -rf "$home"
}
trap cleanup EXIT
unset ROLL_ADMIN_TOKEN
export ROLL_SECRET_KEY=$(head -c 32 /dev/urandom | base64)
L=127.0.0.1
flags=(--home "$home/rolld" --addr-file "$home/addrs.json")
for name in api proxy redis redis-http kafka s3 storage-api queues-amqp queues-stomp queues-mqtt queues-stream nosql-cql \
  queues-management queues-web-stomp queues-web-mqtt queues-http nosql-cql-shard-aware nosql-alternator nosql-rest \
  previews-smtp apps-listen search-http; do
  flags+=(--$name $L:0)
done
"$bin" "${flags[@]}" > "$out/rolld.log" 2>&1 &
pid=$!
for _ in $(seq 120); do
  [[ -s "$home/addrs.json" ]] && break
  kill -0 "$pid" 2>/dev/null || { echo "mcp conformance: rolld exited:" >&2; tail -20 "$out/rolld.log" >&2; exit 1; }
  sleep 1
done
api=http://$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["api"])' "$home/addrs.json")
for _ in $(seq 60); do
  curl -s -m 3 -o /dev/null "$api/v1/mcp" && break
  sleep 1
done

if npx -y "$SUITE" server --url "$api/v1/mcp" --suite all --expected-failures tests/mcp-conformance/expected.yml -o "$out/results" > "$out/suite.txt" 2>&1; then
  grep -E '^Total|expected failure' "$out/suite.txt" | tail -3 || true
  echo "$hash" > "$STAMP"
else
  grep -E '^✗|^Total|unexpected|stale|regression' "$out/suite.txt" | tail -40 >&2 || tail -40 "$out/suite.txt" >&2
  echo "mcp conformance: see target/mcp-conformance/ (suite.txt, results/*/checks.json, rolld.log)" >&2
  exit 1
fi
