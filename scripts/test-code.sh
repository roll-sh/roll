#!/usr/bin/env bash
# roll code's local gate (docs/code/test-coverage.md maps features to tests).
#
# Default (about a minute warm): clippy and the tests of the code crates on SQLite (code-sql,
# roll-cluster, code-store, code-git, code-github, code-index, code-review, code-chat,
# code-control, code-preview's fake runner: git CLI round trips, the HTTP API and its auth matrix,
# GitHub against a mock, the merge queue with a small throughput run, the cluster in one process),
# `roll code` against a live API, then typecheck and the dashboard's and desktop's tests. One
# cargo build for everything (scripts/lib/gate.sh); the test binaries run side by side. The
# cluster layer's suite also runs on DynamoDB Local (`amazon/dynamodb-local` as
# `roll-cloud-cluster-ddb`, removed afterwards) when Docker is there, and skips it otherwise.
#
# Opt in (AGENTS.md says when each must run):
#   --docker  the stores again on Postgres 18 (ROLL_CODE_TEST_DATABASE), checks and previews in real
#             Docker, the Fargate runner's bundles on MinIO, roll code's in-process cluster again
#             with its leases on DynamoDB Local, and a
#             three-node cluster of real `rolld` containers through a kill, a rolling restart and a
#             Postgres restart (crates/rolld/tests/code_cluster_e2e.rs).
#   --stress  40,000 merges through the lander in release (prints merges/s; on Postgres too with
#             --docker) and 2,000 changes through the merge queue.
# Each part runs only when something it covers changed since its last green run; `--force` runs
# them anyway. `CODE_THROUGHPUT_N` and `CODE_QUEUE_N` set the stress sizes.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lib/gate.sh

# roll-auth, roll-access, roll-mcp and roll-net are roll code's gate and evaluator too (docs/permissions.md):
# their suites run here, and with --docker on Postgres like the stores.
CODE_CRATES=(code-sql roll-cluster code-store code-git code-github code-index code-review code-chat code-control code-preview roll-fetch roll-workspace
             roll-auth roll-access roll-mcp roll-net roll-accounts)
RUST_PATHS=("${CODE_CRATES[@]/#/crates/}" crates/agent-proto/src crates/roll-cli/src/code crates/roll-cli/src/code.rs crates/roll-cli/src/main.rs
            crates/roll-cli/src/access.rs crates/roll-cli/tests/code.rs crates/roll-cli/tests/token.rs crates/roll-cli/tests/common crates/roll-cli/Cargo.toml
            docs/code/test-coverage.md Cargo.toml Cargo.lock scripts/test-code.sh scripts/lib/gate.sh)
# Images are pinned by digest (tag@multi-arch index digest): a rebuilt upstream tag can't change them.
DYNAMODB_IMAGE=amazon/dynamodb-local:3.3.2@sha256:6ada631ec7aa5fc83446855eb17233e3dad184fec47477e8140c6c569cfe0c86
POSTGRES_IMAGE=postgres:18-alpine@sha256:123b17998ccaf47e53b502e8c6a1e79afd8f579c6f3eec83c563da026f957395
DOCKER_PATHS=("${RUST_PATHS[@]}" crates/rolld/src crates/rolld/tests/code_cluster_e2e.rs deploy/code)

docker=0 stress=0
for a in "$@"; do
  case "$a" in
    --force) gate_force=1 ;;
    --docker) docker=1 ;;
    --stress) stress=1 ;;
    --unit) ;; # the default now; kept so old invocations still work
    *) echo "code: unknown argument $a" >&2; exit 2 ;;
  esac
done
need_git code

# The default selection: every code crate's unit and integration tests except the stress run
# (code-store's throughput) and real Docker (code-preview's docker), and `roll code`'s.
default_targets() {
  local t=(--lib --bin roll --test code --test token) crate f
  for crate in "${CODE_CRATES[@]}"; do
    for f in crates/"$crate"/tests/*.rs; do
      [[ -e $f ]] || continue
      case "$f" in */code-store/tests/throughput.rs | */code-preview/tests/docker.rs) continue ;; esac
      t+=(--test "$(basename "$f" .rs)")
    done
  done
  printf '%s\n' "${t[@]}"
}
default_args() {
  case "$1:$2:$3" in
    roll-cli:roll:bin) ARGS=(code) ;;
    roll-cli:code:test | roll-cli:token:test) ;;
    code-store:throughput:test | code-preview:docker:test) return 1 ;;
    *) [[ " ${CODE_CRATES[*]} " == *" $1 "* ]] ;;
  esac
}
mapfile -t targets < <(default_targets)

# DynamoDB Local for the cluster layer's DynamoDB backend, shared by every test binary of the run.
ddb=roll-cloud-cluster-ddb
cleanup=()
trap 'for c in "${cleanup[@]}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done' EXIT
start_dynamodb() {
  [[ -n ${ROLL_CLUSTER_TEST_DYNAMODB:-} ]] && return 0
  docker info >/dev/null 2>&1 || return 1
  docker run -d --rm --name "$ddb" -p 127.0.0.1::8000 "$DYNAMODB_IMAGE" >/dev/null || return 1
  cleanup+=("$ddb")
  local port
  port=$(docker port "$ddb" 8000/tcp | head -1 | sed 's/.*://')
  # Docker accepts connections before DynamoDB Local listens: wait for an HTTP answer.
  for _ in $(seq 120); do curl -s -o /dev/null "http://127.0.0.1:$port" && break; sleep 0.5; done
  export ROLL_CLUSTER_TEST_DYNAMODB="http://127.0.0.1:$port"
}

rust_hash=$(hash_of "${RUST_PATHS[@]}")
if stale "$TARGET/.test-code-rust.hash" "$rust_hash"; then
  start_dynamodb || echo "code: no Docker; the cluster layer's DynamoDB tests skip themselves"
  test_args() { default_args "$@"; }
  rust_tests code "${targets[@]}"
  echo "$rust_hash" > "$TARGET/.test-code-rust.hash"
else
  echo "code: Rust unchanged since its last green run; skipping it (--force to run anyway)"
fi

web_tests code

pg_url=""
if [[ $docker == 1 ]]; then
  docker_hash=$(hash_of "${DOCKER_PATHS[@]}")
  if stale "$TARGET/.test-code-docker.hash" "$docker_hash"; then
    if ! docker info >/dev/null 2>&1; then
      echo "code: --docker needs Docker running" >&2
      exit 1
    fi
    # Everything that stores metadata, again on Postgres. Each test makes its own schema.
    pg=roll-code-test-pg-$$
    docker run -d --rm --name "$pg" --shm-size=1g -p 127.0.0.1::5432 -e POSTGRES_USER=roll -e POSTGRES_PASSWORD=roll -e POSTGRES_DB=roll \
      "$POSTGRES_IMAGE" -c max_connections=3000 -c fsync=off -c synchronous_commit=off -c full_page_writes=off >/dev/null
    cleanup+=("$pg")
    port=$(docker port "$pg" 5432/tcp | head -1 | sed 's/.*://')
    for _ in $(seq 60); do docker exec "$pg" pg_isready -U roll -h 127.0.0.1 >/dev/null 2>&1 && break; sleep 1; done
    pg_url="postgres://roll:roll@127.0.0.1:$port/roll?sslmode=disable"
    test_args() {
      case "$1" in code-sql | roll-cluster | code-store | code-control | roll-cli) default_args "$@" ;; *) return 1 ;; esac
    }
    start_dynamodb
    ROLL_CODE_TEST_DATABASE=$pg_url RUST_CLIPPY=0 rust_tests code-pg "${targets[@]}"
    # roll code's cluster with its leases on DynamoDB (data on Postgres).
    test_args() { [[ $1:$2 == code-control:cluster ]]; }
    ROLL_CODE_TEST_CLUSTER_BACKEND=dynamodb ROLL_CLUSTER_DYNAMODB_REQUIRED=1 ROLL_CODE_TEST_DATABASE=$pg_url RUST_CLIPPY=0 rust_tests code-ddb --test cluster
    # Checks and previews in real containers, the Fargate runner's bundles on MinIO (presigned
    # URLs checked by a real S3), and the cluster as real rolld containers (deploy/code/Dockerfile):
    # a kill, a rolling restart and a Postgres restart under load.
    test_args() { [[ $1:$2 == code-preview:docker || $1:$2 == code-preview:fargate || $1:$2 == rolld:code_cluster_e2e ]] && ARGS=(--nocapture); }
    ROLL_CODE_DOCKER_REQUIRED=1 ROLL_E2E=1 RUST_CLIPPY=0 rust_tests code-docker --test docker --test fargate --test code_cluster_e2e
    echo "$docker_hash" > "$TARGET/.test-code-docker.hash"
  else
    echo "code: --docker: nothing it covers changed since its last green run; skipping it (--force to run anyway)"
  fi
fi

if [[ $stress == 1 ]]; then
  stress_hash=$(hash_of "${RUST_PATHS[@]}")
  if stale "$TARGET/.test-code-stress.hash" "$stress_hash"; then
    CODE_THROUGHPUT_N=${CODE_THROUGHPUT_N:-40000} cargo test "${PKGS[@]}" --release --test throughput -- --nocapture
    if [[ -n $pg_url ]]; then
      CODE_THROUGHPUT_N=${CODE_THROUGHPUT_N:-40000} ROLL_CODE_TEST_DATABASE=$pg_url cargo test "${PKGS[@]}" --release --test throughput -- --nocapture
    fi
    CODE_QUEUE_N=${CODE_QUEUE_N:-2000} cargo test "${PKGS[@]}" --test queue throughput -- --nocapture
    echo "$stress_hash" > "$TARGET/.test-code-stress.hash"
  else
    echo "code: --stress: nothing it covers changed since its last green run; skipping it (--force to run anyway)"
  fi
fi

echo "code: all green"
