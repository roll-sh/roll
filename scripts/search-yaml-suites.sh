#!/usr/bin/env bash
# YAML REST suites for roll search (crates/search-yaml).
#
#   scripts/search-yaml-suites.sh [fetch]            download the suites into target/search-yaml/
#   scripts/search-yaml-suites.sh es-baseline        run them against a real Elasticsearch in Docker
#                                                    and rewrite crates/search-yaml/baseline/
#                                                    (ES_JOBS=N: N containers in parallel)
#   scripts/search-yaml-suites.sh os-baseline        the OpenSearch suite against OpenSearch in Docker
#                                                    (crates/search-yaml/baseline/opensearch-*.txt)
#   scripts/search-yaml-suites.sh es-start [security] | es-stop
#                                                    just start (stop) that Elasticsearch on
#                                                    127.0.0.1:${ES_PORT:-19202} (ES_JOBS: several)
#   scripts/search-yaml-suites.sh vendor-clients-tests [ref]
#                                                    refresh crates/search-yaml/suites/clients-tests
#   scripts/search-yaml-suites.sh roll [runner args...]
#                                                    run test/free + clients-tests against roll search
#                                                    (search-dev, $ROLL_JOBS instances in parallel) and
#                                                    compare with baseline/es-9.5.5.txt
#                                                    ($SEARCH_DEV: use that search-dev binary;
#                                                    ROLL_ISOLATED=1: a fresh search-dev process per
#                                                    file instead, via the runner's --spawn)
#   scripts/search-yaml-suites.sh roll-os [runner args...]
#                                                    the OpenSearch suite against search-dev's opensearch
#                                                    dialect, compared with baseline/opensearch-*.txt
#
# Downloaded, never committed (target/ is ignored):
#   target/search-yaml/es-9.5.5/rest-api-spec/{api,test/free,...}   Elasticsearch's rest-resources
#       zip. It is NOT Apache-licensed (Elastic License / SSPL / AGPL), so it only ever lives in
#       target/ on the machine that runs the tests.
#   target/search-yaml/opensearch-3.9.0/{api,test}                   OpenSearch's rest-api-spec
#       (Apache-2.0).
# Vendored: crates/search-yaml/suites/clients-tests (elastic/elasticsearch-clients-tests, Apache-2.0).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

ES_VERSION=${ES_VERSION:-9.5.5}
OS_VERSION=${OS_VERSION:-3.9.0}
OUT=$ROOT/target/search-yaml
ES_DIR=$OUT/es-$ES_VERSION
OS_DIR=$OUT/opensearch-$OS_VERSION
CLIENTS_REF_DEFAULT=9.5
ES_PASSWORD=${ES_PASSWORD:-roll-yaml-test}

fetch_es() {
  if [[ -f $ES_DIR/.complete ]]; then
    echo "search-yaml: Elasticsearch $ES_VERSION suite already in $ES_DIR"
    return
  fi
  local url=https://artifacts.elastic.co/downloads/elasticsearch/rest-resources-zip-$ES_VERSION.zip
  local tmp
  tmp=$(mktemp -d)
  echo "search-yaml: downloading $url"
  curl -fsSL --retry 3 -o "$tmp/rest-resources.zip" "$url"
  if curl -fsSL --retry 3 -o "$tmp/rest-resources.zip.sha512" "$url.sha512" 2>/dev/null; then
    local want got
    want=$(cut -d' ' -f1 <"$tmp/rest-resources.zip.sha512")
    if command -v sha512sum >/dev/null; then
      got=$(sha512sum "$tmp/rest-resources.zip" | cut -d' ' -f1)
    else
      got=$(shasum -a 512 "$tmp/rest-resources.zip" | cut -d' ' -f1)
    fi
    if [[ $want != "$got" ]]; then
      echo "search-yaml: checksum mismatch for $url" >&2
      exit 1
    fi
  fi
  rm -rf "$ES_DIR"
  mkdir -p "$ES_DIR"
  unzip -q "$tmp/rest-resources.zip" -d "$ES_DIR"
  rm -rf "$tmp"
  touch "$ES_DIR/.complete"
  echo "search-yaml: Elasticsearch $ES_VERSION suite in $ES_DIR ($(find "$ES_DIR/rest-api-spec/test/free" -name '*.yml' | wc -l | tr -d ' ') free test files)"
}

fetch_opensearch() {
  if [[ -f $OS_DIR/.complete ]]; then
    echo "search-yaml: OpenSearch $OS_VERSION suite already in $OS_DIR"
    return
  fi
  local tmp
  tmp=$(mktemp -d)
  echo "search-yaml: fetching OpenSearch $OS_VERSION rest-api-spec (sparse clone)"
  git -c advice.detachedHead=false clone -q --depth 1 --filter=blob:none --sparse --branch "$OS_VERSION" \
    https://github.com/opensearch-project/OpenSearch.git "$tmp/os"
  git -C "$tmp/os" sparse-checkout set rest-api-spec/src/main/resources/rest-api-spec
  rm -rf "$OS_DIR"
  mkdir -p "$OS_DIR"
  cp -R "$tmp/os/rest-api-spec/src/main/resources/rest-api-spec/api" "$OS_DIR/api"
  cp -R "$tmp/os/rest-api-spec/src/main/resources/rest-api-spec/test" "$OS_DIR/test"
  cp "$tmp/os/LICENSE.txt" "$tmp/os/NOTICE.txt" "$OS_DIR/" 2>/dev/null || true
  git -C "$tmp/os" rev-parse HEAD >"$OS_DIR/COMMIT"
  rm -rf "$tmp"
  touch "$OS_DIR/.complete"
  echo "search-yaml: OpenSearch $OS_VERSION suite in $OS_DIR"
}

vendor_clients_tests() {
  local ref=${1:-$CLIENTS_REF_DEFAULT}
  local dest=$ROOT/crates/search-yaml/suites/clients-tests
  local sha tmp
  sha=$(git ls-remote https://github.com/elastic/elasticsearch-clients-tests.git "refs/heads/$ref" | cut -f1)
  [[ -n $sha ]] || sha=$ref
  tmp=$(mktemp -d)
  curl -fsSL "https://codeload.github.com/elastic/elasticsearch-clients-tests/tar.gz/$sha" | tar xz -C "$tmp"
  rm -rf "$dest"
  mkdir -p "$dest"
  cp -R "$tmp"/*/tests "$dest/tests"
  cp "$tmp"/*/LICENSE "$tmp"/*/NOTICE "$dest/"
  cat >"$dest/ROLL.md" <<EOF
# elasticsearch-clients-tests (vendored)

Source: https://github.com/elastic/elasticsearch-clients-tests, branch \`$ref\`,
commit \`$sha\`. Apache-2.0 (see LICENSE and NOTICE). Only \`tests/\` is copied, unmodified.

Refresh with \`scripts/search-yaml-suites.sh vendor-clients-tests [branch]\`.
EOF
  rm -rf "$tmp"
  echo "search-yaml: vendored clients-tests $ref ($sha) into $dest"
}

# A CA and a certificate for localhost/127.0.0.1 in target/search-yaml/es-certs, for the HTTPS
# phase (the runner trusts the CA with --ca-cert).
make_certs() {
  local dir=$OUT/es-certs
  [[ -f $dir/node.crt ]] && return
  mkdir -p "$dir"
  openssl req -x509 -newkey rsa:2048 -nodes -keyout "$dir/ca.key" -out "$dir/ca.crt" -days 3650 \
    -subj "/CN=roll search-yaml test CA" 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -keyout "$dir/node.key" -out "$dir/node.csr" -subj "/CN=localhost" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' >"$dir/ext.cnf"
  openssl x509 -req -in "$dir/node.csr" -CA "$dir/ca.crt" -CAkey "$dir/ca.key" -CAcreateserial \
    -out "$dir/node.crt" -days 3650 -extfile "$dir/ext.cnf" 2>/dev/null
  chmod 644 "$dir"/*
}

# start_es [security]: without an argument security is off, as in Elasticsearch's own YAML test
# clusters; `security` turns it on with HTTPS (user elastic / $ES_PASSWORD, certificates from
# make_certs), as the official clients' CI runs elasticsearch-clients-tests.
start_es() {
  local port=${ES_PORT:-19202} name=${ES_CONTAINER:-roll-es-yaml}
  local security=(-e xpack.security.enabled=false) auth=() scheme=http
  if [[ ${1:-} == security ]]; then
    make_certs
    local certs=/usr/share/elasticsearch/config/roll-certs
    security=(-e xpack.security.enabled=true -e "ELASTIC_PASSWORD=$ES_PASSWORD"
      -v "$OUT/es-certs:$certs:ro"
      -e xpack.security.http.ssl.enabled=true -e xpack.security.http.ssl.key=roll-certs/node.key
      -e xpack.security.http.ssl.certificate=roll-certs/node.crt
      -e xpack.security.http.ssl.certificate_authorities=roll-certs/ca.crt
      -e xpack.security.transport.ssl.enabled=false
      -e xpack.security.authc.token.enabled=true -e xpack.security.authc.api_key.enabled=true)
    auth=(-u "elastic:$ES_PASSWORD" --cacert "$OUT/es-certs/ca.crt")
    scheme=https
  fi
  local image=docker.elastic.co/elasticsearch/elasticsearch:$ES_VERSION
  docker image inspect "$image" >/dev/null 2>&1 || docker pull -q "$image"
  stop_es
  # The settings of Elasticsearch's own YAML test clusters (test-clusters DefaultSettingsProvider
  # and the suites' cluster specs): test features, the feature flags ClientYamlTestSuiteIT turns
  # on, remote reindex allowed, plus what the default distribution needs to behave like the
  # integ-test one: a trial license (synthetic source and logsdb are licensed) and path.repo for
  # the fs snapshot repositories.
  local flags="-Dtests.testfeatures.enabled=true -Des.index_mode_feature_flag_registered=true"
  flags+=" -Des.mapping_synthetic_vectors=true -Des.random_sampling_feature_flag_enabled=true"
  flags+=" -Des.logs_stream_feature_flag_enabled=true -Des.slice_indexing_feature_flag_enabled=true"
  docker run --rm -d --name "$name" -m "${ES_MEMORY:-1536m}" -e discovery.type=single-node \
    "${security[@]}" -e xpack.license.self_generated.type=trial \
    -e ES_JAVA_OPTS="-Xms${ES_HEAP:-768m} -Xmx${ES_HEAP:-768m} $flags" \
    -e node.attr.testattr=test -e path.repo=/tmp -e "repositories.url.allowed_urls=http://snapshot.*" \
    -e action.destructive_requires_name=false -e script.disable_max_compilations_rate=true \
    -e indices.breaker.total.use_real_memory=false -e cluster.deprecation_indexing.enabled=false \
    -e cluster.routing.allocation.disk.watermark.low=1b -e cluster.routing.allocation.disk.watermark.high=1b \
    -e cluster.routing.allocation.disk.watermark.flood_stage=1b -e node.processors=2 \
    -e "reindex.remote.whitelist=127.0.0.1:*,172.*:*" -e indices.lifecycle.history_index_enabled=false \
    -e ingest.geoip.downloader.enabled=false \
    -p "127.0.0.1:$port:9200" "$image" >/dev/null
  for _ in $(seq 1 180); do
    if curl -fs "${auth[@]}" "$scheme://127.0.0.1:$port/_cluster/health?wait_for_status=yellow&timeout=1s" >/dev/null 2>&1; then
      if [[ ${1:-} == security ]]; then
        # The superuser Elasticsearch's test clusters define, which some tests log in as. A file
        # realm user, so resetting feature states between tests doesn't remove it.
        docker exec "$name" bin/elasticsearch-users useradd x_pack_rest_user -p x-pack-test-password -r superuser >/dev/null 2>&1
      fi
      return
    fi
    sleep 1
  done
  echo "search-yaml: Elasticsearch did not start on port $port" >&2
  docker logs --tail 50 "$name" >&2 || true
  exit 1
}

# Stops the container and waits until Docker has removed it (--rm removal is asynchronous).
stop_es() {
  local name=${ES_CONTAINER:-roll-es-yaml}
  docker rm -f "$name" >/dev/null 2>&1 || true
  for _ in $(seq 1 60); do
    docker container inspect "$name" >/dev/null 2>&1 || return 0
    sleep 1
  done
}

# ES_JOBS (default 1) identical containers, roll-es-yaml[-i] on consecutive ports from ES_PORT,
# started in parallel; the runner gives each one worker. Each takes ES_MEMORY (1.5 GB).
es_name() { if [[ $1 == 0 ]]; then echo "${ES_CONTAINER:-roll-es-yaml}"; else echo "${ES_CONTAINER:-roll-es-yaml}-$1"; fi; }

start_es_all() {
  local port=${ES_PORT:-19202} pids=() i scheme=http
  if [[ ${1:-} == security ]]; then
    scheme=https
    make_certs
  fi
  ES_URLS=()
  for i in $(seq 0 $((${ES_JOBS:-1} - 1))); do
    ES_CONTAINER=$(es_name "$i") ES_PORT=$((port + i)) start_es "$@" &
    pids+=($!)
    ES_URLS+=(--url "$scheme://127.0.0.1:$((port + i))")
  done
  for i in "${pids[@]}"; do
    wait "$i" || { stop_es_all; exit 1; }
  done
}

stop_es_all() {
  local i
  for i in $(seq 0 $((${ES_JOBS:-1} - 1))); do
    ES_CONTAINER=$(es_name "$i") stop_es &
  done
  wait
}

# Runs `search-yaml run` (the remaining arguments) for a baseline, writing a report ($1) and a
# pass list ($2). Exit status 1 (some tests failed) is expected against a real cluster; anything
# else means the run itself failed and wrote no pass list. Both files are removed first, so a
# pass list left over from an earlier run can never silently become the baseline.
run_for_baseline() {
  local report=$1 pass=$2 status=0
  shift 2
  rm -f "$report" "$pass"
  "$@" --report "$report" --write-baseline "$pass" || status=$?
  if [ "$status" -gt 1 ] || [ ! -f "$pass" ]; then
    echo "search-yaml: the run for $pass failed (exit status $status); not writing a baseline" >&2
    exit 1
  fi
}

# Two phases, both from scratch: test/free with security off (like Elasticsearch's own YAML test
# clusters), then elasticsearch-clients-tests with security and HTTPS on (like the clients' CI). The
# baseline is the union of what passes.
es_baseline() {
  fetch_es
  local base=$ROOT/crates/search-yaml/baseline
  trap stop_es_all EXIT
  cargo build -q -p search-yaml
  local bin=${CARGO_TARGET_DIR:-$ROOT/target}/debug/search-yaml
  local common=(--api-spec "$ES_DIR/rest-api-spec/api" --skip-list "$base/es-$ES_VERSION.skips.txt" --max-failures 100)

  start_es_all
  run_for_baseline "$OUT/es-$ES_VERSION.free.json" "$OUT/es-$ES_VERSION.free.pass.txt" \
    "$bin" run "${ES_URLS[@]}" --suite "free=$ES_DIR/rest-api-spec/test/free" "${common[@]}" "$@"
  stop_es_all

  start_es_all security
  run_for_baseline "$OUT/es-$ES_VERSION.clients.json" "$OUT/es-$ES_VERSION.clients.pass.txt" \
    "$bin" run "${ES_URLS[@]}" --user elastic --password "$ES_PASSWORD" --ca-cert "$OUT/es-certs/ca.crt" \
    --suite "clients=$ROOT/crates/search-yaml/suites/clients-tests/tests" "${common[@]}" "$@"
  stop_es_all

  {
    echo "# Tests that pass on a real single-node Elasticsearch $ES_VERSION in Docker: roll search's target."
    echo "# Written by scripts/search-yaml-suites.sh es-baseline. free = the rest-resources test/free suite,"
    echo "# run with security off and the settings of Elasticsearch's YAML test clusters; clients ="
    echo "# elasticsearch-clients-tests (suites/clients-tests), run with security and HTTPS on (user elastic)."
    echo "# Environment skips, with reasons, are in es-$ES_VERSION.skips.txt."
    echo "# Ids: <suite>:<dir>/<file without .yml>/<test name>."
    sort "$OUT/es-$ES_VERSION.free.pass.txt" "$OUT/es-$ES_VERSION.clients.pass.txt"
  } >"$base/es-$ES_VERSION.txt"
  echo "search-yaml: wrote $base/es-$ES_VERSION.txt ($(grep -vc '^#' "$base/es-$ES_VERSION.txt") passing tests)"
}

start_os() {
  local port=${OS_PORT:-19203} name=${OS_CONTAINER:-roll-os-yaml}
  local image=opensearchproject/opensearch:$OS_VERSION
  docker image inspect "$image" >/dev/null 2>&1 || docker pull -q "$image"
  ES_CONTAINER=$name stop_es
  # OpenSearch's test-cluster settings, security plugin off.
  docker run --rm -d --name "$name" -m "${ES_MEMORY:-1536m}" -e discovery.type=single-node \
    -e DISABLE_SECURITY_PLUGIN=true -e DISABLE_INSTALL_DEMO_CONFIG=true \
    -e OPENSEARCH_JAVA_OPTS="-Xms${ES_HEAP:-768m} -Xmx${ES_HEAP:-768m}" \
    -e node.attr.testattr=test -e path.repo=/tmp -e action.destructive_requires_name=false \
    -e script.disable_max_compilations_rate=true -e indices.breaker.total.use_real_memory=false \
    -e cluster.routing.allocation.disk.watermark.low=1b -e cluster.routing.allocation.disk.watermark.high=1b \
    -e cluster.routing.allocation.disk.watermark.flood_stage=1b -e node.processors=2 \
    -p "127.0.0.1:$port:9200" "$image" >/dev/null
  for _ in $(seq 1 180); do
    curl -fs "http://127.0.0.1:$port/_cluster/health?wait_for_status=yellow&timeout=1s" >/dev/null 2>&1 && return
    sleep 1
  done
  echo "search-yaml: OpenSearch did not start on port $port" >&2
  docker logs --tail 50 "$name" >&2 || true
  exit 1
}

# The OpenSearch suite against a real single-node OpenSearch: crates/search-yaml/baseline/
# opensearch-$OS_VERSION.txt, roll's target for the opensearch dialect.
os_baseline() {
  fetch_opensearch
  local port=${OS_PORT:-19203}
  local base=$ROOT/crates/search-yaml/baseline
  trap 'ES_CONTAINER=${OS_CONTAINER:-roll-os-yaml} stop_es' EXIT
  cargo build -q -p search-yaml
  local bin=${CARGO_TARGET_DIR:-$ROOT/target}/debug/search-yaml
  start_os
  run_for_baseline "$OUT/opensearch-$OS_VERSION.json" "$OUT/opensearch-$OS_VERSION.pass.txt" \
    "$bin" run --url "http://127.0.0.1:$port" --suite "os=$OS_DIR/test" --api-spec "$OS_DIR/api" \
    --skip-list "$base/opensearch-$OS_VERSION.skips.txt" --max-failures 100 "$@"
  {
    echo "# Tests of OpenSearch's YAML suite (rest-api-spec, Apache-2.0) that pass on a real single-node"
    echo "# OpenSearch $OS_VERSION in Docker (security plugin off): roll search's target for the opensearch"
    echo "# dialect. Written by scripts/search-yaml-suites.sh os-baseline; environment skips, with"
    echo "# reasons, are in opensearch-$OS_VERSION.skips.txt."
    sort "$OUT/opensearch-$OS_VERSION.pass.txt"
  } >"$base/opensearch-$OS_VERSION.txt"
  echo "search-yaml: wrote $base/opensearch-$OS_VERSION.txt ($(grep -vc '^#' "$base/opensearch-$OS_VERSION.txt") passing tests)"
}

# roll search under test: $ROLL_JOBS search-dev processes (in memory, one cluster each) on
# ports from $ROLL_PORT; the runner gives every process one worker and spreads the
# files across them. Extra arguments go to `search-yaml run` (--filter, --max-failures, ...).
ROLL_PIDS=()
stop_roll() {
  for pid in "${ROLL_PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${ROLL_PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done
  ROLL_PIDS=()
}

start_roll() {
  # Ports below the Linux ephemeral range (32768+), where outgoing connections can't hold them;
  # a port that is taken anyway is skipped.
  local dialect=$1 bin=$2 jobs=${ROLL_JOBS:-4} port=${ROLL_PORT:-19310}
  ROLL_URLS=()
  for _ in $(seq 1 "$jobs"); do
    local started=
    for _ in $(seq 1 20); do
      local log=$OUT/runs/search-dev-$port.log
      "$bin" --listen "127.0.0.1:$port" --dialect "$dialect" --yaml-test-cluster >"$log" 2>&1 &
      local pid=$!
      for _ in $(seq 1 100); do
        if curl -fs "http://127.0.0.1:$port/" >/dev/null 2>&1 && kill -0 "$pid" 2>/dev/null; then
          started=1
          break
        fi
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
      done
      if [[ -n $started ]]; then
        ROLL_PIDS+=("$pid")
        ROLL_URLS+=(--url "http://127.0.0.1:$port")
        port=$((port + 1))
        break
      fi
      kill "$pid" 2>/dev/null || true
      port=$((port + 1))
    done
    if [[ -z $started ]]; then
      echo "search-yaml: search-dev did not start (see $OUT/runs/search-dev-*.log)" >&2
      exit 1
    fi
  done
}

roll_run() {
  local dialect=$1
  shift
  mkdir -p "$OUT/runs"
  local target=${CARGO_TARGET_DIR:-$ROOT/target}/debug
  # $SEARCH_DEV: an already-built search-dev binary (skips building search-server).
  if [[ -n ${SEARCH_DEV:-} ]]; then
    cargo build -q -p search-yaml
  else
    cargo build -q -p search-yaml -p search-server --bin search-yaml --bin search-dev
  fi
  local base=$ROOT/crates/search-yaml/baseline
  local dev=${SEARCH_DEV:-$target/search-dev}
  if [[ -n ${ROLL_ISOLATED:-} ]]; then
    # One search-dev per file: state one file leaves behind can't fail the next; $ROLL_JOBS run
    # at a time on free ports the runner picks.
    ROLL_URLS=(--spawn "'$dev' --listen 127.0.0.1:{port} --dialect $dialect --yaml-test-cluster" --jobs "${ROLL_JOBS:-4}")
  else
    trap stop_roll EXIT
    start_roll "$dialect" "$dev"
  fi
  local status=0
  if [[ $dialect == elasticsearch ]]; then
    fetch_es
    "$target/search-yaml" run "${ROLL_URLS[@]}" --api-spec "$ES_DIR/rest-api-spec/api" \
      --suite "free=$ES_DIR/rest-api-spec/test/free" --suite "clients=$ROOT/crates/search-yaml/suites/clients-tests/tests" \
      --skip-list "$base/es-$ES_VERSION.skips.txt" --baseline "$base/es-$ES_VERSION.txt" \
      --report "$OUT/roll.json" --quiet "$@" || status=$?
  else
    fetch_opensearch
    "$target/search-yaml" run "${ROLL_URLS[@]}" --api-spec "$OS_DIR/api" --suite "os=$OS_DIR/test" \
      --skip-list "$base/opensearch-$OS_VERSION.skips.txt" --baseline "$base/opensearch-$OS_VERSION.txt" \
      --report "$OUT/roll-opensearch.json" --quiet "$@" || status=$?
  fi
  stop_roll
  return $status
}

cmd=${1:-fetch}
[[ $# -gt 0 ]] && shift
case "$cmd" in
  fetch) fetch_es; fetch_opensearch ;;
  fetch-es) fetch_es ;;
  fetch-opensearch) fetch_opensearch ;;
  vendor-clients-tests) vendor_clients_tests "$@" ;;
  es-baseline) es_baseline "$@" ;;
  es-start) start_es_all "$@"; echo "search-yaml: Elasticsearch on ${ES_URLS[*]}" ;;
  os-baseline) os_baseline "$@" ;;
  es-stop) stop_es_all ;;
  roll) roll_run elasticsearch "$@" ;;
  roll-os) roll_run opensearch "$@" ;;
  *) echo "usage: $0 [fetch|fetch-es|fetch-opensearch|es-baseline|es-start|es-stop|os-baseline|roll|roll-os|vendor-clients-tests [ref]]" >&2; exit 2 ;;
esac
