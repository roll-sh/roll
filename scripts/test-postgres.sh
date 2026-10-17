#!/usr/bin/env bash
# Postgres test suite: unit tests, clippy and end-to-end tests against real computes in Docker.
#
# Only runs when something Postgres-related changed since the last green run (the crates, the
# workspace manifest/lockfile, this script or the CI workflow). `--force` runs regardless;
# `--unit` skips the Docker tests. Extra args go to the e2e test binary (e.g. a test name).
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/pg-proxy crates/pg-storage crates/pg-control crates/rolld crates/roll-cli
       Cargo.toml Cargo.lock scripts/test-postgres.sh .github/workflows/postgres.yml)
STAMP=target/.test-postgres.hash

force=0 unit_only=0 args=()
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --unit) unit_only=1 ;;
    *) args+=("$a") ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "postgres: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

CRATES=(-p pg-proxy -p pg-storage -p pg-control -p rolld -p roll-cli)
cargo build "${CRATES[@]}"
cargo clippy "${CRATES[@]}" --all-targets -- -D warnings
cargo test "${CRATES[@]}" --lib --bins

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "postgres: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  for v in 16 17 18; do
    docker image inspect "postgres:$v-alpine" >/dev/null 2>&1 || docker pull -q "postgres:$v-alpine"
  done
  ROLL_E2E=1 cargo test -p pg-control --test e2e -- --test-threads="${ROLL_E2E_THREADS:-4}" ${args[@]+"${args[@]}"}
fi

# Only a full run counts as green.
if [[ $unit_only == 0 && ${#args[@]} == 0 ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "postgres: all green"
