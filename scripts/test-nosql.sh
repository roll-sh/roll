#!/usr/bin/env bash
# roll nosql test suite: unit and integration tests, clippy, the differential and driver tests
# against real ScyllaDB 2026.3.3 and Cassandra 5.0.9 in Docker, and (with --conformance) ScyllaDB's
# cqlpy suite against roll, checked against the reference baselines.
#
# Only runs when something nosql-related changed since the last green run. `--force` runs
# regardless; `--unit` skips everything that needs Docker. Reference containers already running
# are reused when ROLL_NOSQL_REF_SCYLLA / ROLL_NOSQL_REF_CASSANDRA (and the *_AUTH variants, and
# ROLL_NOSQL_REF_SCYLLA_ALTERNATOR for ScyllaDB's Alternator) are set; otherwise this script starts
# them and removes them on exit. Set CARGO to wrap cargo (e.g. a
# memory-limited wrapper). Set ROLL_E2E_S3_BUCKET (+ ROLL_E2E_S3_ENDPOINT, AWS_* credentials) to
# run the persistence tests against S3 instead of a local directory.
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/nosql-types crates/nosql-proto crates/nosql-cql crates/nosql-storage crates/nosql-engine
       crates/nosql-alternator crates/nosql-server crates/nosql-control crates/nosql-sstable-import crates/pg-storage crates/rolld crates/roll-cli
       Cargo.toml Cargo.lock scripts/test-nosql.sh scripts/nosql-conformance.sh scripts/nosql-conformance deploy/nosql
       .github/workflows/nosql.yml)
STAMP=target/.test-nosql.hash
CARGO=${CARGO:-cargo}

force=0 unit_only=0 conformance=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --unit) unit_only=1 ;;
    --conformance) conformance=1 ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "nosql: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

CRATES=(-p nosql-types -p nosql-proto -p nosql-cql -p nosql-storage -p nosql-engine -p nosql-alternator
        -p nosql-server -p nosql-control -p nosql-sstable-import)
$CARGO build "${CRATES[@]}" -p rolld -p roll-cli
$CARGO clippy "${CRATES[@]}" -p rolld -p roll-cli --all-targets --all-features -- -D warnings
# Everything that needs no reference server: env-gated reference tests skip themselves. With
# ROLL_E2E_S3_BUCKET set, every suite on test_objects (and rolld's) runs against S3.
$CARGO test "${CRATES[@]}" --all-features
$CARGO test -p roll-cli --features roll-server-tests --test nosql_api --test cqlsh_roll
$CARGO test -p rolld --test nosql

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "nosql: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  started=()
  cleanup() { [[ ${#started[@]} -gt 0 ]] && docker rm -f "${started[@]}" >/dev/null 2>&1 || true; }
  trap cleanup EXIT
  wait_cql() { # container, cqlsh args...
    local c=$1; shift
    for _ in $(seq 1 120); do docker exec "$c" cqlsh "$@" -e "select release_version from system.local" >/dev/null 2>&1 && return; sleep 2; done
    echo "nosql: $c did not start" >&2; docker logs "$c" 2>&1 | tail -20 >&2; exit 1
  }
  if [[ -z ${ROLL_NOSQL_REF_SCYLLA:-} ]]; then
    docker run -d --rm --name nosql-test-scylla -p 127.0.0.1::9042 -p 127.0.0.1::10000 -p 127.0.0.1::8000 scylladb/scylla:2026.3.3 \
      --smp 1 --memory 1G --overprovisioned 1 --developer-mode 1 --api-address 0.0.0.0 \
      --alternator-address 0.0.0.0 --alternator-port 8000 --alternator-write-isolation always_use_lwt >/dev/null
    started+=(nosql-test-scylla); wait_cql nosql-test-scylla
    export ROLL_NOSQL_REF_SCYLLA=$(docker port nosql-test-scylla 9042 | head -1)
    export ROLL_NOSQL_REF_SCYLLA_REST=$(docker port nosql-test-scylla 10000 | head -1)
    export ROLL_NOSQL_REF_SCYLLA_ALTERNATOR=http://$(docker port nosql-test-scylla 8000 | head -1)
  fi
  if [[ -z ${ROLL_NOSQL_REF_CASSANDRA:-} ]]; then
    docker run -d --rm --name nosql-test-cassandra --memory 2560m -p 127.0.0.1::9042 \
      -e MAX_HEAP_SIZE=640M -e HEAP_NEWSIZE=128M cassandra:5.0.9 >/dev/null
    started+=(nosql-test-cassandra); wait_cql nosql-test-cassandra
    export ROLL_NOSQL_REF_CASSANDRA=$(docker port nosql-test-cassandra 9042 | head -1)
  fi
  export NOSQL_PROTO_REF_SCYLLA=$ROLL_NOSQL_REF_SCYLLA NOSQL_PROTO_REF_CASSANDRA=$ROLL_NOSQL_REF_CASSANDRA

  $CARGO test -p nosql-proto --features client --test reference
  $CARGO test -p nosql-server --test compat --test drivers --test protocol --test rest --test alternator_compat
  $CARGO test -p nosql-control --test imports --test import_sstables
  $CARGO test -p roll-cli --test cqlsh --test cqlsh_live

  # The roll/nosql image (deploy/nosql): its configuration mapping, the tools (Testcontainers,
  # golang-migrate, cassandra-stress, scylla-bench, the references' cqlsh) and the Python, Java,
  # Go and Node drivers, against the image built from this tree unless ROLL_NOSQL_IMAGE names one.
  if [[ -z ${ROLL_NOSQL_IMAGE:-} ]]; then
    deploy/nosql/build-local.sh --tag roll/nosql:test
    export ROLL_NOSQL_IMAGE=roll/nosql:test
  fi
  for c in "${started[@]}"; do
    case "$c" in
      nosql-test-scylla) export ROLL_NOSQL_REF_SCYLLA_CONTAINER=$c ;;
      nosql-test-cassandra) export ROLL_NOSQL_REF_CASSANDRA_CONTAINER=$c ;;
    esac
  done
  $CARGO test -p nosql-server --test image --test tools -- --test-threads=1
  $CARGO test -p nosql-server --test drivers docker_drivers

  if [[ $conformance == 1 ]]; then
    scripts/nosql-conformance.sh --roll scylla --compare crates/nosql-server/tests/conformance/reference-scylla-cqlpy.txt
    scripts/nosql-conformance.sh --roll cassandra --compare crates/nosql-server/tests/conformance/reference-cassandra-cqlpy.txt
    scripts/nosql-conformance.sh --roll scylla --suite alternator --compare crates/nosql-server/tests/conformance/reference-scylla-alternator.txt
  fi
fi

# Only a full run counts as green.
if [[ $unit_only == 0 && -z "${ROLL_COMPAT_FILTER:-}" ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "nosql: all green"
