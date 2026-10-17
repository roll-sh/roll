#!/usr/bin/env bash
# Stream test suite: unit tests, clippy, the wire-protocol, storage, compaction, HTTP and
# librdkafka tests, and the Java client/tools tests against apache/kafka in Docker.
#
# Only runs when something stream-related changed since the last green run (the crates, the
# workspace manifest/lockfile, this script or the CI workflow). `--force` runs regardless;
# `--unit` skips the Docker tests. Extra args go to the Java test binary (e.g. a test name).
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/stream-log crates/stream-broker crates/pg-storage crates/rolld crates/roll-cli
       Cargo.toml Cargo.lock scripts/test-stream.sh .github/workflows/stream.yml)
STAMP=target/.test-stream.hash
KAFKA_IMAGE=${ROLL_E2E_KAFKA_IMAGE:-apache/kafka:4.1.0}

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
  echo "stream: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

CRATES=(-p stream-log -p stream-broker -p rolld -p roll-cli)
cargo build "${CRATES[@]}"
cargo clippy "${CRATES[@]}" --all-targets -- -D warnings
# Everything that needs no Docker (the Java tests skip themselves without ROLL_E2E).
cargo test "${CRATES[@]}"

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "stream: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  docker image inspect "$KAFKA_IMAGE" >/dev/null 2>&1 || docker pull -q "$KAFKA_IMAGE"
  ROLL_E2E=1 cargo test -p stream-broker --test java -- --test-threads="${ROLL_E2E_THREADS:-4}" ${args[@]+"${args[@]}"}
fi

# Only a full run counts as green.
if [[ $unit_only == 0 && ${#args[@]} == 0 ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "stream: all green"
