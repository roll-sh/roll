#!/usr/bin/env bash
# roll queues test suite: build, clippy, unit and in-process integration tests for every queues
# crate, then the Docker end-to-end tests (real client libraries, RabbitMQ itself, MinIO,
# failover, rebalance and upgrade under load) and the client compatibility matrix.
#
# Only runs when something queues-related changed since the last green run (the mq-* crates,
# rolld, the CLI, the workspace manifest/lockfile, the client matrix, the deploy image and
# template, this script or the CI workflow). `--force` runs regardless; `--unit` skips Docker. Extra args go to `cargo test`.
set -euo pipefail
cd "$(dirname "$0")/.."

MQ=($(ls -d crates/mq-* 2>/dev/null))
PATHS=("${MQ[@]}" crates/rolld crates/roll-cli tests/queues deploy/queues Cargo.toml Cargo.lock
       scripts/test-queues.sh scripts/test-queues-clients.sh .github/workflows/queues.yml)
STAMP=target/.test-queues.hash

force=0 unit_only=0 args=()
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --unit) unit_only=1 ;;
    *) args+=("$a") ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' -not -path '*/node_modules/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "queues: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

CRATES=()
for c in "${MQ[@]}"; do CRATES+=(-p "$(basename "$c")"); done
CRATES+=(-p rolld -p roll-cli)

cargo build "${CRATES[@]}"
cargo clippy "${CRATES[@]}" --all-targets -- -D warnings
cargo test "${CRATES[@]}" ${args[@]+"${args[@]}"}

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "queues: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  # Docker-backed tests skip themselves unless ROLL_E2E=1.
  ROLL_E2E=1 cargo test "${CRATES[@]}" -- --test-threads="${ROLL_E2E_THREADS:-4}" ${args[@]+"${args[@]}"}
  if [[ -x scripts/test-queues-clients.sh ]]; then
    scripts/test-queues-clients.sh
  fi
fi

# Only a full run counts as green.
if [[ $unit_only == 0 && ${#args[@]} == 0 ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "queues: all green"
