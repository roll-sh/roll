#!/usr/bin/env bash
# roll tasks test suite: build (incl. the Lambda feature), clippy, every test on SQLite and on the
# log store (in memory), then in Docker: the log store on S3 (MinIO: conditional writes for
# commits and the lease), and DynamoDB Local with large bodies in S3 (SeaweedFS).
#
# Only runs when something tasks-related changed since the last green run. `--force` runs
# regardless; `--unit` skips the Docker run. Extra args go to cargo test (e.g. a test name).
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/tasks crates/rolld crates/roll-cli Cargo.toml Cargo.lock
       scripts/test-tasks.sh .github/workflows/tasks.yml deploy/aws)
STAMP=target/.test-tasks.hash

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
  echo "tasks: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

cargo build -p tasks -p roll-cli
cargo build -p tasks --features lambda
cargo clippy -p tasks -p roll-cli --all-targets --features lambda -- -D warnings
cargo test -p tasks ${args[@]+"${args[@]}"}
ROLL_TASKS_E2E_LOG=memory:// cargo test -p tasks --tests ${args[@]+"${args[@]}"}

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "tasks: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  ddb=roll-tasks-ddb-$$ s3=roll-tasks-s3-$$ minio=roll-tasks-minio-$$
  trap 'docker rm -f "$ddb" "$s3" "$minio" >/dev/null 2>&1 || true' EXIT

  # The log store on S3.
  docker run -d --rm --name "$minio" -p 127.0.0.1::9000 -e MINIO_ROOT_USER=roll -e MINIO_ROOT_PASSWORD=roll-tasks-test \
    cgr.dev/chainguard/minio server /data >/dev/null
  minio_port=$(docker port "$minio" 9000/tcp | head -1 | sed 's/.*://')
  for _ in $(seq 60); do
    curl -sf -o /dev/null "http://127.0.0.1:$minio_port/minio/health/live" && break
    sleep 1
  done
  docker run --rm --network "container:$minio" -e MC_HOST_l=http://roll:roll-tasks-test@127.0.0.1:9000 \
    cgr.dev/chainguard/minio-client mb l/roll-tasks-log >/dev/null
  AWS_ACCESS_KEY_ID=roll AWS_SECRET_ACCESS_KEY=roll-tasks-test AWS_REGION=us-east-1 \
  AWS_ENDPOINT_URL="http://127.0.0.1:$minio_port" ROLL_TASKS_E2E_LOG=s3://roll-tasks-log/e2e \
    cargo test -p tasks --tests ${args[@]+"${args[@]}"}
  docker rm -f "$minio" >/dev/null 2>&1 || true

  # DynamoDB.
  docker run -d --rm --name "$ddb" -p 127.0.0.1::8000 amazon/dynamodb-local -jar DynamoDBLocal.jar -inMemory -sharedDb >/dev/null
  docker run -d --rm --name "$s3" -p 127.0.0.1::8333 chrislusf/seaweedfs server -s3 >/dev/null
  ddb_port=$(docker port "$ddb" 8000/tcp | head -1 | sed 's/.*://')
  s3_port=$(docker port "$s3" 8333/tcp | head -1 | sed 's/.*://')
  for _ in $(seq 60); do
    curl -s -o /dev/null "http://127.0.0.1:$ddb_port" && curl -s -o /dev/null "http://127.0.0.1:$s3_port" && break
    sleep 1
  done
  for _ in $(seq 30); do
    code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT "http://127.0.0.1:$s3_port/roll-tasks-e2e" || true)
    [[ $code == 200 || $code == 409 ]] && break
    sleep 1
  done
  AWS_ACCESS_KEY_ID=local AWS_SECRET_ACCESS_KEY=local AWS_REGION=us-east-1 \
  ROLL_TASKS_E2E_DYNAMO="http://127.0.0.1:$ddb_port" \
  ROLL_TASKS_E2E_S3_BUCKET=roll-tasks-e2e ROLL_TASKS_E2E_S3_ENDPOINT="http://127.0.0.1:$s3_port" \
    cargo test -p tasks ${args[@]+"${args[@]}"}
fi

# Only a full run counts as green.
if [[ $unit_only == 0 && ${#args[@]} == 0 ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "tasks: all green"
