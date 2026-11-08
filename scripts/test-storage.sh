#!/usr/bin/env bash
# roll storage test suite: build, clippy, the unit and integration tests of the s3 crates, and a
# compatibility suite with real S3 tools in Docker (aws cli, rclone, mc, s3cmd).
#
# Only runs when something storage-related changed since the last green run. `--force` runs
# regardless; `--unit` skips the Docker tools suite. Set ROLL_E2E_S3_BUCKET (+ ROLL_E2E_S3_ENDPOINT,
# AWS_* credentials) to also run the store and server tests against S3 instead of a local directory.
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/s3-proto crates/s3-select crates/s3-image crates/s3-store crates/s3-server crates/s3-control crates/s3-api crates/pg-storage crates/roll-cluster crates/rolld crates/roll-cli
       Cargo.toml Cargo.lock scripts/test-storage.sh .github/workflows/storage.yml)
STAMP=target/.test-storage.hash
# Images are pinned by digest (tag@multi-arch index digest): a rebuilt upstream tag can't change them.
DYNAMODB_IMAGE=amazon/dynamodb-local:3.3.2@sha256:6ada631ec7aa5fc83446855eb17233e3dad184fec47477e8140c6c569cfe0c86
# The S3 tools; the same as crates/s3-server/tests/tools.rs.
TOOL_IMAGES=(amazon/aws-cli:2.39.6@sha256:eccdb3ec77e159d43ca6a750a70738c546883138be7f094498294a1477ec7e42
             rclone/rclone:1.76.0@sha256:7cbeec24a44f206067597c0368be4de90ae19a64d5113c7d1938bd737a81e466
             "${ROLL_MC_IMAGE:-bitnamilegacy/minio-client:2025.5.21@sha256:00dcc4e58ada0df45bb7d9ee435af98295f96c27c3c68292ce78ec700a87b511}"
             d3fk/s3cmd:latest@sha256:04757666dffdb68311bc62eb9bdf13bb7f5aeb4a386a0c074cb39bd8f35cf5df)

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

CRATES=(-p s3-proto -p s3-select -p s3-image -p s3-store -p s3-server -p s3-control -p s3-api)

# DynamoDB Local for the cluster tests' DynamoDB runs (tests/cluster.rs; they skip without
# Docker). ROLL_CLUSTER_TEST_DYNAMODB set: use that one.
ddb=roll-cloud-storage-ddb-$$
trap 'docker rm -f "$ddb" >/dev/null 2>&1 || true' EXIT
if [[ -z ${ROLL_CLUSTER_TEST_DYNAMODB:-} ]] && docker info >/dev/null 2>&1; then
  docker run -d --rm --name "$ddb" -p 127.0.0.1::8000 "$DYNAMODB_IMAGE" >/dev/null
  port=$(docker port "$ddb" 8000/tcp | head -1 | sed 's/.*://')
  # Docker accepts connections before DynamoDB Local listens: wait for an HTTP answer.
  for _ in $(seq 120); do curl -s -o /dev/null "http://127.0.0.1:$port" && break; sleep 0.5; done
  export ROLL_CLUSTER_TEST_DYNAMODB="http://127.0.0.1:$port"
fi

cargo build "${CRATES[@]}" -p rolld -p roll-cli
cargo clippy "${CRATES[@]}" -p rolld -p roll-cli --all-targets -- -D warnings
cargo test -p s3-proto
cargo test -p s3-select
cargo test -p s3-image
cargo test -p s3-store
# Everything except the Docker tools suite.
cargo test -p s3-server --lib
for t in crates/s3-server/tests/*.rs; do
  [[ -e $t ]] || continue
  name=$(basename "$t" .rs)
  # The Docker suites: real tools below, LocalStack in scripts/test-storage-aws.sh, the CDN
  # Worker in scripts/test-storage-cdn.sh.
  [[ $name == tools || $name == aws || $name == cdn_worker ]] && continue
  cargo test -p s3-server --test "$name"
done
cargo test -p s3-control
# The developer API (Supabase-Storage-compatible): routes, rules, limits, resumable uploads.
cargo test -p s3-api
# `roll storage …` end to end (and `roll storage accounts|iam …`).
cargo test -p roll-cli --test storage --test iam

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "storage: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  for img in "${TOOL_IMAGES[@]}"; do
    docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img"
  done
  ROLL_COMPAT_REQUIRED=1 cargo test -p s3-server --test tools
fi

# Only a full run counts as green.
if [[ $unit_only == 0 ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "storage: all green"
