#!/usr/bin/env bash
# The client matrix: unmodified official clients, each in its own Docker image, run a
# representative scenario (index with mappings, bulk and the bulk helpers, search with
# aggregations, scroll / scan and point in time, update and delete by query, errors, the
# client's product check) against roll search; searchkick runs its own test suite, and the
# framework integrations (django-elasticsearch-dsl, Laravel Scout) a smoke test each.
#
#   tests/search/clients/run.sh [client…]     (default: all; names as in CLIENTS below)
#
# By default it builds search-dev and starts two in-memory instances on the host — the
# elasticsearch dialect on $ROLL_PORT (default 19720) and the opensearch dialect on
# $ROLL_PORT+1 — which the containers reach with --network host. Set CLIENTS_ES_URL and
# CLIENTS_OS_URL to run against servers that are already up instead (a real Elasticsearch
# 9.5.5 / OpenSearch 3.9 makes a baseline: every client passes there). SEARCH_DEV=<binary>
# skips the build.
#
# Prints one line per client and a summary; exits non-zero when any client fails. Each
# client's output is in target/search-clients/<client>.log.
set -euo pipefail
cd "$(dirname "$0")/../../.."
ROOT=$PWD
DIR=tests/search/clients
OUT=target/search-clients
mkdir -p "$OUT"

# name  image-dir  script  dialect (es|os)
CLIENTS=(
  "python      python      es_client.py  es"
  "python-os   python      os_client.py  os"
  "javascript  javascript  es_client.mjs es"
  "javascript-os javascript os_client.mjs os"
  "java        java        -             es"
  "go          go          -             es"
  "ruby        ruby        es_client.rb  es"
  "php         php         es_client.php es"
  "django      django      -             es"
  "laravel     laravel     -             es"
  "searchkick  searchkick  -             es"
)

want=("$@")
selected() {
  [[ ${#want[@]} == 0 ]] && return 0
  local w
  for w in "${want[@]}"; do [[ $w == "$1" ]] && return 0; done
  return 1
}

DOCKER=(docker)
if ! docker info >/dev/null 2>&1; then
  if command -v sg >/dev/null && sg docker -c 'docker info' >/dev/null 2>&1; then
    DOCKER=(sg_docker)
  else
    echo "clients: Docker is not available" >&2
    exit 1
  fi
fi
sg_docker() {
  local q=() a
  for a in "$@"; do q+=("$(printf '%q' "$a")"); done
  sg docker -c "docker ${q[*]}"
}

pids=()
cleanup() {
  local p
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

wait_up() {
  local url=$1 i
  for i in $(seq 100); do
    curl -s -o /dev/null "$url" && return 0
    sleep 0.2
  done
  echo "clients: $url did not answer" >&2
  return 1
}

port=${ROLL_PORT:-19720}
ES_URL=${CLIENTS_ES_URL:-}
OS_URL=${CLIENTS_OS_URL:-}
if [[ -z $ES_URL || -z $OS_URL ]]; then
  dev=${SEARCH_DEV:-}
  if [[ -z $dev ]]; then
    cargo build -q -p search-server --bin search-dev
    dev=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/search-dev
  fi
  if [[ -z $ES_URL ]]; then
    "$dev" --listen "127.0.0.1:$port" > "$OUT/search-dev-es.log" 2>&1 &
    pids+=($!)
    ES_URL=http://127.0.0.1:$port
  fi
  if [[ -z $OS_URL ]]; then
    "$dev" --listen "127.0.0.1:$((port + 1))" --dialect opensearch > "$OUT/search-dev-os.log" 2>&1 &
    pids+=($!)
    OS_URL=http://127.0.0.1:$((port + 1))
  fi
fi
wait_up "$ES_URL"
wait_up "$OS_URL"

built=()
build() {
  local dir=$1 b
  for b in "${built[@]}"; do [[ $b == "$dir" ]] && return 0; done
  echo "clients: building roll-search-client-$dir"
  DOCKER_BUILDKIT=1 "${DOCKER[@]}" build -q -t "roll-search-client-$dir" "$ROOT/$DIR/$dir" > "$OUT/build-$dir.log" 2>&1 || {
    echo "clients: building $dir failed (see $OUT/build-$dir.log)" >&2
    return 1
  }
  built+=("$dir")
}

run() {
  local name=$1 dir=$2 script=$3 dialect=$4 log=$OUT/$1.log
  local env=(-e "ES_URL=$ES_URL" -e "OS_URL=$OS_URL" -e "ELASTICSEARCH_URL=$ES_URL")
  local args=()
  [[ $script != - ]] && args=("$script")
  [[ $dialect == os ]] && env+=(-e "ELASTICSEARCH_URL=$OS_URL")
  "${DOCKER[@]}" run --rm --network host -m 3g "${env[@]}" "roll-search-client-$dir" "${args[@]}" > "$log" 2>&1
}

passed=() failed=()
for c in "${CLIENTS[@]}"; do
  read -r name dir script dialect <<<"$c"
  selected "$name" || continue
  if ! build "$dir"; then
    failed+=("$name")
    printf '%-14s FAIL (image build)\n' "$name"
    continue
  fi
  start=$SECONDS
  ok=0
  run "$name" "$dir" "$script" "$dialect" && ok=1
  # searchkick names indices by creation time in milliseconds: two reindexes in the same
  # millisecond collide (`resource_already_exists_exception`) against any server. One retry.
  if [[ $ok == 0 && $name == searchkick ]] && grep -q resource_already_exists_exception "$OUT/$name.log"; then
    run "$name" "$dir" "$script" "$dialect" && ok=1
  fi
  summary=$(grep -E 'all checks passed|check\(s\) failed|[0-9]+ runs, ' "$OUT/$name.log" | tail -1 || true)
  if [[ $ok == 1 ]]; then
    passed+=("$name")
    printf '%-14s ok    %4ss  %s\n' "$name" $((SECONDS - start)) "$summary"
  else
    failed+=("$name")
    printf '%-14s FAIL  %4ss  %s (see %s)\n' "$name" $((SECONDS - start)) "$summary" "$OUT/$name.log"
  fi
done

echo "clients: ${#passed[@]} passed, ${#failed[@]} failed${failed:+: ${failed[*]}}"
[[ ${#failed[@]} == 0 ]]
