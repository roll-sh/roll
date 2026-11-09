#!/usr/bin/env bash
# The roll cloud fleet end to end (`pnpm test:fleet`; docs/cloud/README.md, "Testing the fleet"):
# builds the fleet image (dev profile), brings up the local fleet (deploy/cloud/local: three rolld
# nodes, DynamoDB Local, Postgres, MinIO, HAProxy as the NLB), runs crates/rolld/tests/fleet_e2e.rs
# against it (every product under load through a SIGKILL, a rolling restart and a scale-out), and
# tears the stack down. Takes about 15 minutes.
#
#   --no-build   use the image already built (ROLL_E2E_IMAGE, default roll-cloud-e2e:dev)
#   --keep       leave the stack (and the image) up afterwards
#
# Everything it creates is named roll-cloud-e2e-*: the containers, their volumes, the network
# roll-cloud-e2e-net and the image; that is all it removes. On a failure the stack's logs are saved
# to $ROLL_E2E_WORK/fleet.log. Ports on 127.0.0.1 (ROLL_E2E_*_PORT in the compose file move them):
# 28443 HTTPS, 26379 Redis, 29092 Kafka, 29042 CQL, 25671 AMQPS, 28000 DynamoDB, 28404 HAProxy stats.
set -euo pipefail
cd "$(dirname "$0")/.."

build=1 keep=0
for a in "$@"; do
  case "$a" in
    --no-build) build=0 ;;
    --keep) keep=1 ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done

if ! docker info >/dev/null 2>&1; then
  echo "fleet: Docker is not running" >&2
  exit 1
fi
if ! git --version >/dev/null 2>&1; then
  echo "fleet: the test drives the real git client; install git" >&2
  exit 1
fi

export ROLL_E2E_IMAGE=${ROLL_E2E_IMAGE:-roll-cloud-e2e:dev}
work=${ROLL_E2E_WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/roll-cloud-e2e}
mkdir -p "$work"
export ROLL_E2E_CERTS=$work/certs
file=deploy/cloud/local/docker-compose.yml
# `--profile scale` takes node4 (the scale-out) down with the rest.
compose=(docker compose -f "$file" --profile scale)

deploy/cloud/local/gen-certs.sh "$ROLL_E2E_CERTS"

if [[ $build == 1 ]]; then
  echo "fleet: building $ROLL_E2E_IMAGE (dev profile)"
  docker build --build-arg PROFILE=dev ${CARGO_BUILD_JOBS:+--build-arg CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS} \
    -f deploy/cloud/Dockerfile -t "$ROLL_E2E_IMAGE" .
fi

# The names are fixed: what an interrupted run left goes first.
"${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true

cleanup() {
  local status=$?
  if [[ $status != 0 ]]; then
    "${compose[@]}" logs --no-color --timestamps >"$work/fleet.log" 2>&1 || true
    echo "fleet: the stack's logs are in $work/fleet.log" >&2
  fi
  if [[ $keep == 0 ]]; then
    "${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
    if [[ $build == 1 ]]; then docker rmi "$ROLL_E2E_IMAGE" >/dev/null 2>&1 || true; fi
  else
    echo "fleet: --keep: the stack is still up (${compose[*]} down -v to remove it)"
  fi
  exit "$status"
}
trap cleanup EXIT

docker compose -f "$file" up -d --wait --wait-timeout 300

ROLL_E2E=1 cargo test -p rolld --features fleet-e2e --test fleet_e2e -- --nocapture
