#!/usr/bin/env bash
# roll storage against real AWS APIs without AWS credentials: LocalStack (community edition,
# pinned) in Docker for S3, KMS, SQS, SNS, Lambda and EventBridge. Runs
# crates/s3-server/tests/aws.rs: SSE-KMS with the backing bucket on LocalStack S3, notifications
# to SQS/SNS/Lambda/EventBridge, replication to buckets outside roll (streamed and server-side),
# and Object Lambda with a Lambda function.
#
# Only runs when something storage-related changed since the last green run; `--force` runs
# regardless. Needs Docker (Lambda functions run as containers through LocalStack's socket).
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/s3-proto crates/s3-store crates/s3-server crates/s3-control crates/pg-storage
       Cargo.toml Cargo.lock scripts/test-storage-aws.sh)
STAMP=target/.test-storage-aws.hash
IMAGE=localstack/localstack:4.9

force=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "storage-aws: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

if ! docker info >/dev/null 2>&1; then
  echo "storage-aws: Docker is not running" >&2
  exit 1
fi
docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull -q "$IMAGE"
# The tests start LocalStack (and a relay for Lambda functions) themselves; clean up after.
cleanup() { docker rm -f roll-localstack-tests roll-localstack-relay >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup

export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1
ROLL_AWS_TESTS=1 cargo test -p s3-server --test aws -- --test-threads 2

mkdir -p target && echo "$hash" > "$STAMP"
echo "storage-aws: all green"
