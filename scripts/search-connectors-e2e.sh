#!/usr/bin/env bash
# roll search's connector flows against real servers: Postgres (postgres:18-alpine) and an S3
# store (MinIO), each on 127.0.0.1 with -m 1g, removed afterwards. Runs
# crates/search-es/tests/connectors.rs (it skips itself without these servers).
#
#   scripts/search-connectors-e2e.sh            # build and run
#   CARGO_TARGET_DIR=… scripts/search-connectors-e2e.sh
set -euo pipefail
cd "$(dirname "$0")/.."

docker_() { if docker info >/dev/null 2>&1; then docker "$@"; else sg docker -c "docker $(printf '%q ' "$@")"; fi; }

tag="roll-search-connectors-$$"
pg="$tag-pg" s3="$tag-s3"
cleanup() { docker_ rm -f "$pg" "$s3" >/dev/null 2>&1 || true; }
trap cleanup EXIT

docker_ run -d --name "$pg" -m 1g -p 127.0.0.1::5432 \
  -e POSTGRES_USER=roll -e POSTGRES_PASSWORD=pg-secret-7731 -e POSTGRES_DB=shop postgres:18-alpine >/dev/null
docker_ run -d --name "$s3" -m 1g -p 127.0.0.1::9000 \
  -e MINIO_ROOT_USER=rollminio -e MINIO_ROOT_PASSWORD=s3-secret-5523 -e MINIO_DEFAULT_BUCKETS=docs bitnamilegacy/minio:latest >/dev/null

port() { docker_ port "$1" "$2" | head -1 | sed 's/.*://'; }
pg_port=$(port "$pg" 5432/tcp)
s3_port=$(port "$s3" 9000/tcp)

for _ in $(seq 1 60); do
  docker_ exec "$pg" pg_isready -U roll -d shop >/dev/null 2>&1 && break
  sleep 1
done
for _ in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$s3_port/minio/health/ready" >/dev/null 2>&1 && break
  sleep 1
done
# The default bucket is made after MinIO is ready.
sleep 2

export ROLL_E2E_CONNECTORS_PG="127.0.0.1:$pg_port"
export ROLL_E2E_CONNECTORS_S3="http://127.0.0.1:$s3_port"
cargo test -p search-es --test connectors -- --test-threads 1
