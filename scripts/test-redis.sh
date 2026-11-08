#!/usr/bin/env bash
# Redis test suite: unit and integration tests, the cluster tests (memory and DynamoDB Local),
# clippy, and the differential compatibility suite
# (every reply compared with a real Redis 8 in Docker).
#
# Only runs when something Redis-related changed since the last green run. `--force` runs
# regardless; `--unit` skips the Docker compatibility suite. `ROLL_COMPAT_FILTER=lists` runs one
# compat script. Set ROLL_E2E_S3_BUCKET (+ ROLL_E2E_S3_ENDPOINT, AWS_* credentials) to also run
# the durability tests against S3 instead of a local directory.
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/redis-proto crates/redis-lua crates/redis-engine crates/redis-server crates/redis-control crates/rolld crates/roll-cli
       crates/roll-cluster crates/roll-edge crates/pg-storage crates/code-sql
       Cargo.toml Cargo.lock scripts/test-redis.sh .github/workflows/redis.yml)
STAMP=target/.test-redis.hash
# Images are pinned by digest (tag@multi-arch index digest): a rebuilt upstream tag can't change them.
COMPAT_IMAGE=${ROLL_COMPAT_IMAGE:-redis:8.10.3-alpine@sha256:71885d8cc6e42f2bc0aa2eb853bfc4be200b5108a46ac50e117ae1df0531fb80}
DYNAMODB_IMAGE=amazon/dynamodb-local:3.3.2@sha256:6ada631ec7aa5fc83446855eb17233e3dad184fec47477e8140c6c569cfe0c86

force=0 unit_only=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --unit) unit_only=1 ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "redis: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

CRATES=(-p redis-proto -p redis-lua -p redis-engine -p redis-server -p redis-control)
cargo build "${CRATES[@]}" -p rolld -p roll-cli
cargo clippy "${CRATES[@]}" -p rolld -p roll-cli --all-targets -- -D warnings

# DynamoDB Local for the cluster tests' DynamoDB backend (they run on the memory backend too;
# without Docker the DynamoDB ones skip themselves).
ddb=roll-cloud-redis-ddb
trap 'docker rm -f "$ddb" >/dev/null 2>&1 || true' EXIT
start_dynamodb() {
  [[ -n ${ROLL_CLUSTER_TEST_DYNAMODB:-} ]] && return 0
  docker info >/dev/null 2>&1 || return 1
  docker rm -f "$ddb" >/dev/null 2>&1 || true
  docker run -d --rm --name "$ddb" -p 127.0.0.1::8000 "$DYNAMODB_IMAGE" >/dev/null || return 1
  local port
  port=$(docker port "$ddb" 8000/tcp | head -1 | sed 's/.*://')
  # Docker accepts connections before DynamoDB Local listens: wait for an HTTP answer.
  for _ in $(seq 120); do curl -s -o /dev/null "http://127.0.0.1:$port" && break; sleep 0.5; done
  export ROLL_CLUSTER_TEST_DYNAMODB="http://127.0.0.1:$port" ROLL_CLUSTER_DYNAMODB_REQUIRED=1
}
start_dynamodb || echo "redis: no Docker; the cluster tests' DynamoDB runs skip themselves"

# Everything except the Docker compat suite.
cargo test "${CRATES[@]}" --lib --bins
cargo test -p redis-server --test http --test persist --test node --test repl --test migrate
# The control plane, and three nodes in one process (redis_control::cluster).
cargo test -p redis-control --tests

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "redis: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  docker image inspect "$COMPAT_IMAGE" >/dev/null 2>&1 || docker pull -q "$COMPAT_IMAGE"
  ROLL_COMPAT_REQUIRED=1 cargo test -p redis-server --test compat
  # Redis' replication protocol against real Redis (its own container).
  ROLL_COMPAT_REQUIRED=1 cargo test -p redis-server --test redis_repl
fi

# Only a full run counts as green.
if [[ $unit_only == 0 && -z "${ROLL_COMPAT_FILTER:-}" ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "redis: all green"
