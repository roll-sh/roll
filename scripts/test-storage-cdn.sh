#!/usr/bin/env bash
# roll storage's CDN: the Cloudflare Worker (apps/cdn-worker) and roll's side of it.
#
# - the Worker's unit tests (node:test: JWTs, SigV4 presigned URLs, cache keys, routes, tags);
# - crates/s3-server/tests/cdn.rs: the gateway cache, the origin's checks, purges to a mock
#   Cloudflare API, usage reports (in process);
# - crates/s3-server/tests/cdn_worker.rs: the Worker in workerd (Miniflare, in a node
#   container on the host network) in front of roll: edge token checks, caching, purge on
#   change, immutable and mutable URLs, cached egress.
#
# Only runs when something CDN-related changed since the last green run; `--force` runs
# regardless. Needs Docker and pnpm.
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/s3-proto crates/s3-store crates/s3-server crates/s3-control apps/cdn-worker/src apps/cdn-worker/test
       apps/cdn-worker/package.json apps/cdn-worker/wrangler.toml Cargo.toml Cargo.lock pnpm-lock.yaml scripts/test-storage-cdn.sh)
STAMP=target/.test-storage-cdn.hash
IMAGE=node:22-bookworm-slim

force=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' -not -path '*/node_modules/*' -not -path '*/dist/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "storage-cdn: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

if ! docker info >/dev/null 2>&1; then
  echo "storage-cdn: Docker is not running" >&2
  exit 1
fi
[[ -d apps/cdn-worker/node_modules/miniflare ]] || pnpm install --frozen-lockfile --filter @roll/cdn-worker...
docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull -q "$IMAGE"

pnpm --filter @roll/cdn-worker typecheck
pnpm --filter @roll/cdn-worker test
cargo test -p s3-server --test cdn
ROLL_CDN_REQUIRED=1 cargo test -p s3-server --test cdn_worker

mkdir -p target && echo "$hash" > "$STAMP"
echo "storage-cdn: all green"
