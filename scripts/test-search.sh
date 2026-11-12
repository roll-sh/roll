#!/usr/bin/env bash
# roll search test suite: build, clippy and the tests of every search-* crate plus rolld and the
# CLI, the dashboard's search helpers (vitest), then (unless --unit) the Docker-backed compatibility checks:
#   - Elastic's YAML REST suites against search-dev (scripts/search-yaml-suites.sh roll), the
#     OpenSearch suite against its opensearch dialect (roll-os), the SQL plugin's PPL suite
#     (roll-os-sql) and Elasticsearch's module and
#     x-pack suites (roll-extended, one minimum per suite), each held to a minimum pass count
#     from docs/search/compatibility.md;
#   - the differential suite against a real Elasticsearch, when present
#     (crates/search-server/tests/differential.rs);
#   - the client matrix of unmodified official clients, when present (tests/search/clients/run.sh).
#
# Only runs when something search-related changed since the last green run. `--force` runs
# regardless; `--unit` skips the Docker checks. Set ROLL_E2E_S3_BUCKET (+ ROLL_E2E_S3_ENDPOINT,
# AWS_* credentials) to run the storage and control-plane tests against S3 instead of a local
# directory, and ROLL_CODE_TEST_DATABASE (a Postgres URL) to run the catalog on Postgres.
# ROLL_PORT / ROLL_JOBS are passed to the YAML runner (search-dev ports and processes).
#
# Pass thresholds: docs/search/compatibility.md lines of the form
#   <!-- test-search: roll >= 1234 -->
#   <!-- test-search: roll-os >= 567 -->
#   <!-- test-search: roll-os-sql >= 204 -->      (the OpenSearch SQL plugin's PPL suite)
#   <!-- test-search: lang-painless >= 89 -->      (an extended suite, crates/search-yaml/suites/extended.txt)
# (a missing line means no minimum; the counts are printed either way).
set -euo pipefail
cd "$(dirname "$0")/.."

CRATES=()
for d in crates/search-*/; do
  CRATES+=(-p "$(basename "$d")")
done
PATHS=(crates/search-* crates/snowball crates/pg-storage crates/roll-cluster crates/code-sql crates/rolld crates/roll-cli
       Cargo.toml Cargo.lock scripts/test-search.sh scripts/search-yaml-suites.sh
       docs/search/compatibility.md .github/workflows/search.yml tests/search
       apps/dashboard/src/lib/search-console.ts apps/dashboard/src/lib/search-docs.ts
       apps/dashboard/src/lib/search-console.test.ts apps/dashboard/src/lib/search-docs.test.ts)
STAMP=target/.test-search.hash

force=0 unit_only=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --unit) unit_only=1 ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done

if command -v sha256sum >/dev/null; then SHA=(sha256sum); else SHA=(shasum -a 256); fi
# Optional paths (compatibility.md, the workflow) may not exist yet: find then exits non-zero,
# which pipefail would turn into a silent exit.
hash=$({ find "${PATHS[@]}" -type f -not -path '*/target/*' 2>/dev/null || true; } | LC_ALL=C sort | xargs "${SHA[@]}" | "${SHA[@]}" | cut -d' ' -f1)
if [[ $force == 0 && -f $STAMP && "$(cat "$STAMP")" == "$hash" ]]; then
  echo "search: no changes since the last green run; skipping (--force to run anyway)"
  exit 0
fi

cargo build "${CRATES[@]}" -p rolld -p roll-cli
cargo clippy "${CRATES[@]}" -p rolld -p roll-cli --all-targets -- -D warnings
cargo test "${CRATES[@]}"
# rolld's own tests check the flags (clap only reports a clash between flattened groups at run
# time) and run rolld with every product.
cargo test -p roll-cli -p rolld

# The dashboard's console completion and document helpers.
pnpm --filter dashboard exec vitest run src/lib/search-console.test.ts src/lib/search-docs.test.ts

# Minimum pass count for a suite from docs/search/compatibility.md (0 when not set).
threshold() {
  local f=docs/search/compatibility.md
  [[ -f $f ]] || { echo 0; return; }
  grep -oE "test-search: $1 >= [0-9]+" "$f" | head -1 | grep -oE '[0-9]+$' || echo 0
}

# Passed tests in a search-yaml JSON report.
passed() {
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["totals"]["passed"])' "$1"
}

yaml_suite() {
  local mode=$1 report=$2 min
  min=$(threshold "$mode")
  # The run exits non-zero whenever roll passes fewer tests than the real cluster's baseline,
  # which it will until it is complete: the gate is the minimum pass count below. A run that
  # fails before writing its report (a build error, search-dev not starting) must not be judged
  # on the previous run's report.
  rm -f "$report"
  scripts/search-yaml-suites.sh "$mode" --max-failures 1000000 || true
  [[ -f $report ]] || { echo "search: $mode produced no report ($report)" >&2; exit 1; }
  local n
  n=$(passed "$report")
  echo "search: YAML $mode: $n passed (minimum $min)"
  if (( n < min )); then
    echo "search: YAML $mode fell below its minimum: $n < $min (docs/search/compatibility.md)" >&2
    exit 1
  fi
}

# The extended suites (crates/search-yaml/suites/extended.txt), each held to its own minimum.
extended_suites() {
  local tsv=target/search-yaml/roll-extended.tsv suite tests es roll rest min failed=0
  rm -f "$tsv"
  scripts/search-yaml-suites.sh roll-extended -- --max-failures 0 || true
  [[ -f $tsv ]] || { echo "search: roll-extended produced no table ($tsv)" >&2; exit 1; }
  while read -r suite tests es roll rest; do
    [[ $suite == suite ]] && continue
    min=$(threshold "$suite")
    if (( roll < min )); then
      echo "search: YAML $suite fell below its minimum: $roll < $min (docs/search/compatibility.md)" >&2
      failed=1
    fi
  done <"$tsv"
  # A suite with a minimum must have run.
  for suite in $(grep -oE 'test-search: [a-z0-9.-]+ >=' docs/search/compatibility.md | awk '{print $2}'); do
    [[ $suite == roll || $suite == roll-os || $suite == roll-os-sql ]] && continue
    grep -q "^$suite	" "$tsv" || { echo "search: extended suite $suite has a minimum but didn't run" >&2; failed=1; }
  done
  (( failed == 0 )) || exit 1
  echo "search: extended YAML suites at or above their minimums"
}

if [[ $unit_only == 0 ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "search: Docker is not running; start it or pass --unit" >&2
    exit 1
  fi
  yaml_suite roll target/search-yaml/roll.json
  yaml_suite roll-os target/search-yaml/roll-opensearch.json
  yaml_suite roll-os-sql target/search-yaml/roll-opensearch-sql.json
  extended_suites
  if [[ -f crates/search-server/tests/differential.rs ]]; then
    ROLL_E2E=1 cargo test -p search-server --test differential
  fi
  if [[ -x tests/search/clients/run.sh ]]; then
    tests/search/clients/run.sh
  fi
fi

# Only a full run counts as green.
if [[ $unit_only == 0 ]]; then
  mkdir -p target && echo "$hash" > "$STAMP"
fi
echo "search: all green"
