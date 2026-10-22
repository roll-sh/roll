#!/usr/bin/env bash
# roll storage test suite: build, clippy, the unit and integration tests of the s3 crates, and a
# compatibility suite with real S3 tools in Docker (aws cli, rclone, mc, s3cmd).
#
# Only runs when something storage-related changed since the last green run. `--force` runs
# regardless; `--unit` skips the Docker tools suite. Set ROLL_E2E_S3_BUCKET (+ ROLL_E2E_S3_ENDPOINT,
# AWS_* credentials) to also run the store and server tests against S3 instead of a local directory.
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/s3-proto crates/s3-store crates/s3-server crates/s3-control crates/pg-storage crates/rolld crates/roll-cli
       Cargo.toml Cargo.lock scripts/test-storage.sh .github/workflows/storage.yml)
STAMP=target/.test-storage.hash

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
  echo "storage: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

CRATES=(-p s3-proto -p s3-store -p s3-server -p s3-control)
cargo build "${CRATES[@]}" -p rolld -p roll-cli
cargo clippy "${CRATES[@]}" -p rolld -p roll-cli --all-targets -- -D warnings
cargo test -p s3-proto
cargo test -p s3-store
# Everything except the Docker tools suite.
cargo test -p s3-server --lib
for t in crates/s3-server/tests/*.rs; do
  [[ -e $t ]] || continue
  name=$(basename "$t" .rs)
  # The Docker suites: real tools below, LocalStack in scripts/test-storage-aws.sh.
  [[ $name == tools || $name == aws ]] && continue
  cargo test -p s3-server --test "$name"
done
cargo test -p s3-control
# `roll storage …` end to end.
cargo test -p roll-cli --test storage

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "storage: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  for img in amazon/aws-cli rclone/rclone "${ROLL_MC_IMAGE:-bitnamilegacy/minio-client}" d3fk/s3cmd; do
    docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img"
  done
  ROLL_COMPAT_REQUIRED=1 cargo test -p s3-server --test tools
fi

# Only a full run counts as green.
if [[ $unit_only == 0 ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "storage: all green"
