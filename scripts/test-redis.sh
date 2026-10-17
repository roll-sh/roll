#!/usr/bin/env bash
# Redis test suite: unit and integration tests, clippy, and the differential compatibility suite
# (every reply compared with a real Redis 8 in Docker).
#
# Only runs when something Redis-related changed since the last green run. `--force` runs
# regardless; `--unit` skips the Docker compatibility suite. `ROLL_COMPAT_FILTER=lists` runs one
# compat script. Set ROLL_E2E_S3_BUCKET (+ ROLL_E2E_S3_ENDPOINT, AWS_* credentials) to also run
# the durability tests against S3 instead of a local directory.
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/redis-proto crates/redis-lua crates/redis-engine crates/redis-server crates/redis-control crates/rolld crates/roll-cli
       Cargo.toml Cargo.lock scripts/test-redis.sh .github/workflows/redis.yml)
STAMP=target/.test-redis.hash

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
# Everything except the Docker compat suite.
cargo test "${CRATES[@]}" --lib --bins
cargo test -p redis-server --test http --test persist --test node --test repl --test migrate
cargo test -p redis-control --tests

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "redis: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  docker image inspect "${ROLL_COMPAT_IMAGE:-redis:8-alpine}" >/dev/null 2>&1 || docker pull -q "${ROLL_COMPAT_IMAGE:-redis:8-alpine}"
  ROLL_COMPAT_REQUIRED=1 cargo test -p redis-server --test compat
fi

# Only a full run counts as green.
if [[ $unit_only == 0 && -z "${ROLL_COMPAT_FILTER:-}" ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "redis: all green"
