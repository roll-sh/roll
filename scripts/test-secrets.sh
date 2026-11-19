#!/usr/bin/env bash
# roll secrets' local gate (docs/secrets.md): roll-secrets' tests on SQLite (the store, crypto and
# every route's flows, two nodes on one database), rolld's (the deployment key, roll apps' resolve,
# and the product links: each of rolld's link providers against the real rolld with its products in
# process, crates/rolld/tests/secrets.rs), every route's access class (`rolld --test auth`),
# `roll secrets` and `roll run` (with `--watch`) against roll secrets in process, then typecheck and
# the dashboard's secrets tests. Clippy and the build share the package set (scripts/lib/gate.sh).
#
#   --docker  roll-secrets' flows and nodes again on Postgres 18 (ROLL_CODE_TEST_DATABASE, a
#             container removed afterwards), and the product links with Postgres (its computes run
#             in Docker; ROLL_SECRETS_DOCKER=1).
#
# Each part runs only when something it covers changed since its last green run; `--force` runs
# them anyway.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lib/gate.sh

RUST_PATHS=(crates/roll-secrets crates/roll-oidc crates/roll-workspace crates/rolld/src/secrets.rs crates/rolld/src/secrets
            crates/rolld/tests/secrets.rs crates/rolld/tests/auth.rs crates/roll-access/src/catalog.rs crates/roll-access/src/prefix-reviewed.txt
            crates/roll-cli/src/secrets.rs crates/roll-cli/src/mask.rs crates/roll-cli/tests/secrets.rs
            Cargo.toml Cargo.lock scripts/test-secrets.sh scripts/lib/gate.sh)
WEB_PATHS_SECRETS=(apps/dashboard/src/lib apps/dashboard/src/components/secrets apps/dashboard/package.json apps/dashboard/vitest.config.mts
                   pnpm-lock.yaml scripts/test-secrets.sh)
# Pinned by digest (tag@multi-arch index digest), as scripts/test-code.sh's.
POSTGRES_IMAGE=postgres:18-alpine@sha256:123b17998ccaf47e53b502e8c6a1e79afd8f579c6f3eec83c563da026f957395

docker=0
for a in "$@"; do
  case "$a" in
    --force) gate_force=1 ;;
    --docker) docker=1 ;;
    *) echo "secrets: unknown argument $a" >&2; exit 2 ;;
  esac
done

TARGETS=(--lib --bin roll --bin rolld --test api --test nodes --test secrets --test auth)
# Which test binaries are secrets' (same-named targets in other packages are skipped).
test_args() {
  case "$1:$2:$3" in
    roll-secrets:*) ;;
    rolld:rolld:bin) ARGS=(secrets) ;;
    rolld:secrets:test | rolld:auth:test | roll-cli:secrets:test) ;;
    roll-cli:roll:bin) ARGS=(secrets:: mask::) ;;
    *) return 1 ;;
  esac
}

rust_hash=$(hash_of "${RUST_PATHS[@]}")
if stale "$TARGET/.test-secrets-rust.hash" "$rust_hash"; then
  rust_tests secrets "${TARGETS[@]}"
  echo "$rust_hash" > "$TARGET/.test-secrets-rust.hash"
else
  echo "secrets: Rust unchanged since its last green run; skipping it (--force to run anyway)"
fi

web_hash=$(hash_of "${WEB_PATHS_SECRETS[@]}")
if stale "$TARGET/.test-secrets-web.hash" "$web_hash"; then
  pnpm typecheck
  # Every test file whose path names a secret: lib/secret*.test.ts and components/secrets/.
  pnpm --filter dashboard exec vitest run secret
  echo "$web_hash" > "$TARGET/.test-secrets-web.hash"
else
  echo "secrets: dashboard unchanged since its last green run; skipping it (--force to run anyway)"
fi

if [[ $docker == 1 ]]; then
  docker_hash=$(hash_of "${RUST_PATHS[@]}" crates/code-sql crates/rolld/src/main.rs)
  if stale "$TARGET/.test-secrets-docker.hash" "$docker_hash"; then
    if ! docker info >/dev/null 2>&1; then
      echo "secrets: --docker needs Docker running" >&2
      exit 1
    fi
    # A fresh database each run: every test makes its own schema, and nothing is left behind.
    pg=roll-secrets-test-pg-$$
    trap 'docker rm -f "$pg" >/dev/null 2>&1 || true' EXIT
    docker run -d --rm --name "$pg" --shm-size=1g -p 127.0.0.1::5432 -e POSTGRES_USER=roll -e POSTGRES_PASSWORD=roll -e POSTGRES_DB=roll \
      "$POSTGRES_IMAGE" -c max_connections=3000 -c fsync=off -c synchronous_commit=off -c full_page_writes=off >/dev/null
    port=$(docker port "$pg" 5432/tcp | head -1 | sed 's/.*://')
    for _ in $(seq 60); do docker exec "$pg" pg_isready -U roll -h 127.0.0.1 >/dev/null 2>&1 && break; sleep 1; done
    test_args() { [[ $1:$2 == roll-secrets:api || $1:$2 == roll-secrets:nodes ]]; }
    ROLL_CODE_TEST_DATABASE="postgres://roll:roll@127.0.0.1:$port/roll?sslmode=disable" RUST_CLIPPY=0 rust_tests secrets-pg --test api --test nodes
    # rolld's computes (Postgres 18 by default).
    docker image inspect postgres:18-alpine >/dev/null 2>&1 || docker pull -q postgres:18-alpine
    test_args() { [[ $1:$2 == rolld:secrets ]]; }
    ROLL_SECRETS_DOCKER=1 RUST_CLIPPY=0 rust_tests secrets-docker --test secrets
    echo "$docker_hash" > "$TARGET/.test-secrets-docker.hash"
  else
    echo "secrets: --docker: nothing it covers changed since its last green run; skipping it (--force to run anyway)"
  fi
fi
echo "secrets: green"
