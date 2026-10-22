#!/usr/bin/env bash
# roll code test suite: unit and integration tests and clippy for the code-* crates (git CLI
# round-trips, the HTTP API, GitHub against a mock), `roll code` against a live API, the
# dashboard's unit tests (vitest), the checks/previews tests in Docker, and the release-mode
# landing throughput test (40,000 merges; prints merges/s) and 2,000 changes through the merge
# queue. docs/code/test-coverage.md maps features to tests.
#
# Only runs when something code-related changed since the last green run. `--force` runs
# regardless; `--unit` skips the Docker tests and the throughput runs. `CODE_THROUGHPUT_N` and
# `CODE_QUEUE_N` set their sizes.
set -euo pipefail
cd "$(dirname "$0")/.."

PATHS=(crates/code-store crates/code-git crates/code-github crates/code-index crates/code-review crates/code-control crates/code-preview crates/rolld crates/roll-cli
       apps/dashboard/src apps/dashboard/package.json apps/dashboard/vitest.config.mts
       Cargo.toml Cargo.lock scripts/test-code.sh)
STAMP=target/.test-code.hash

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
  echo "code: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

if ! git --version >/dev/null 2>&1; then
  echo "code: the tests drive the real git client; install git" >&2
  exit 1
fi

CRATES=(-p code-store -p code-git -p code-github -p code-index -p code-review -p code-control -p code-preview)
cargo build "${CRATES[@]}" -p rolld -p roll-cli
# rolld is linted by the other suites; linting it here would also lint every service it links.
cargo clippy "${CRATES[@]}" -p roll-cli --all-targets -- -D warnings
# One crate at a time keeps peak memory down. The Docker tests skip themselves here.
for c in "${CRATES[@]}"; do
  [[ $c == -p ]] && continue
  cargo test -p "$c"
done
cargo test -p roll-cli
# The dashboard: pure logic (API client, event stream, diffs, git proxy, 404 proxy) in node, and
# components (diff review, markdown, queue, blame, tokens) in jsdom.
pnpm --filter dashboard test

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "code: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  ROLL_CODE_DOCKER_REQUIRED=1 cargo test -p code-preview --test docker
  CODE_THROUGHPUT_N=${CODE_THROUGHPUT_N:-40000} cargo test -p code-store --release --test throughput -- --nocapture
  CODE_QUEUE_N=${CODE_QUEUE_N:-2000} cargo test -p code-control --test queue throughput -- --nocapture
fi

# Only a full run counts as green.
if [[ $unit_only == 0 ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "code: all green"
