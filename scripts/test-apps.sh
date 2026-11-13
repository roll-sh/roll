#!/usr/bin/env bash
# roll apps test suite (docs/apps/README.md): clippy and the tests of apps-build and apps-control
# (the API and the waking proxy in process, on the fake runtime), the CLI's and rolld's build, and
# the dashboard's typecheck and tests. `--docker` adds the real Docker run: a build from an upload
# and the cold-start, resume and warm timings (crates/apps-control/tests/docker.rs), which print a
# MEASURED line for docs/apps/README.md, and custom domains' HTTP-01 certificates from Pebble.
#
# Only runs when something apps-related changed since the last green run. `--force` runs
# regardless. Extra args go to cargo test.
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/apps-build crates/apps-control crates/roll-workspace crates/rolld/src/apps.rs crates/rolld/src/edge.rs crates/roll-cli/src/apps.rs
       crates/roll-edge
       crates/roll-fetch apps/dashboard/src/components/apps apps/dashboard/src/lib/apps-api.ts
       apps/dashboard/src/lib/workspace-resources.ts Cargo.toml Cargo.lock scripts/test-apps.sh)
STAMP=target/.test-apps.hash

force=0 docker=0 args=()
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --docker) docker=1 ;;
    *) args+=("$a") ;;
  esac
done

hash=$(find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null | LC_ALL=C sort | xargs shasum -a 256 | shasum -a 256 | cut -d' ' -f1)
if [[ $force == 0 && $docker == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "apps: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

cargo clippy -p apps-build -p apps-control -p roll-edge --all-targets -- -D warnings
cargo test -p apps-build -p apps-control -p roll-workspace "${args[@]}"
# Custom domains' certificates at the edge: the HTTP-01 manager, the plain-HTTP listener, and
# Cloudflare for SaaS against a fake Cloudflare.
cargo test -p roll-edge --lib --test custom_domains
cargo build -p rolld -p roll-cli
if [[ $docker == 1 ]]; then
  ROLL_APPS_DOCKER=1 cargo test -p apps-control --test docker -- --nocapture
  # HTTP-01 from Pebble (Let's Encrypt's test CA) against the edge's plain-HTTP listener.
  ROLL_APPS_DOCKER=1 cargo test -p roll-edge --test custom_domains
fi
pnpm typecheck
pnpm --filter dashboard test
echo "$hash" > "$STAMP"
echo "apps: green"
