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
#   scripts/search-yaml-suites.sh roll-os-sql [runner args...]
#                                                    the OpenSearch SQL plugin's PPL suite against the
#                                                    opensearch dialect (baseline/opensearch-*-sql.txt)
#
# The extended suites: Elasticsearch's module, plugin and x-pack YAML suites outside test/free,
# listed with their test clusters' configuration in crates/search-yaml/suites/extended.txt:
#   scripts/search-yaml-suites.sh fetch-extended     download them into target/search-yaml/es-*-src/
#   scripts/search-yaml-suites.sh es-extended-baseline [suite...] [-- runner args...]
#                                                    run them against private Elasticsearch clusters
#                                                    set up like each suite's own (ES_JOBS per group,
#                                                    default 4) and write baseline/es-*-<suite>.txt
#   scripts/search-yaml-suites.sh roll-extended [suite...] [-- runner args...]
#                                                    run them against search-dev (same settings and
#                                                    users, roll's security emulation) and print per
#                                                    suite: tests, ES passes, roll passes, missing;
#                                                    also in target/search-yaml/roll-extended.tsv
#   scripts/search-yaml-suites.sh extended-list [suite...] | extended-check [suite...]
#                                                    print the suite definitions | check them and
#                                                    count each suite's tests
#
# Downloaded, never committed (target/ is ignored):
#   target/search-yaml/es-9.5.5/rest-api-spec/{api,test/free,...}   Elasticsearch's rest-resources
#       zip. It is NOT Apache-licensed (Elastic License / SSPL / AGPL), so it only ever lives in
#       target/ on the machine that runs the tests.
#   target/search-yaml/es-9.5.5-src/{modules,plugins,x-pack}/...     the yamlRestTest resources of
#       Elasticsearch's v9.5.5 source tree (same licence: never committed).
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

# The OpenSearch SQL plugin's YAML suite (opensearch-project/sql, integ-test/src/yamlRestTest,
# Apache-2.0: PPL issues, ppl and api tests plus the plugin's own API spec), for the PPL engine.
OS_SQL_DIR=$OUT/opensearch-$OS_VERSION-sql
fetch_opensearch_sql() {
  if [[ -f $OS_SQL_DIR/.complete ]]; then
    echo "search-yaml: OpenSearch SQL plugin $OS_VERSION.0 suite already in $OS_SQL_DIR"
    return
  fi
  local tmp
  tmp=$(mktemp -d)
  git -c advice.detachedHead=false clone -q --depth 1 --filter=blob:none --sparse --branch "$OS_VERSION.0" \
    https://github.com/opensearch-project/sql.git "$tmp/sql"
  git -C "$tmp/sql" sparse-checkout set integ-test/src/yamlRestTest/resources/rest-api-spec
  rm -rf "$OS_SQL_DIR"
  mkdir -p "$OS_SQL_DIR"
  cp -R "$tmp/sql/integ-test/src/yamlRestTest/resources/rest-api-spec/api" "$OS_SQL_DIR/api"
  cp -R "$tmp/sql/integ-test/src/yamlRestTest/resources/rest-api-spec/test" "$OS_SQL_DIR/test"
  git -C "$tmp/sql" rev-parse HEAD >"$OS_SQL_DIR/COMMIT"
  rm -rf "$tmp"
  touch "$OS_SQL_DIR/.complete"
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

# ES_ARGS: the `docker run` arguments of Elasticsearch's own YAML test clusters (test-clusters
# DefaultSettingsProvider and the suites' cluster specs): test features, the feature flags
# ClientYamlTestSuiteIT turns on, remote reindex allowed, plus what the default distribution needs
# to behave like the integ-test one: a license ($1: trial, as synthetic source and logsdb are
# licensed, or basic) and path.repo for the fs snapshot repositories. $ES_JVM_EXTRA: more JVM
# options.
es_test_cluster_args() {
  local flags="-Dtests.testfeatures.enabled=true -Des.index_mode_feature_flag_registered=true"
  flags+=" -Des.mapping_synthetic_vectors=true -Des.random_sampling_feature_flag_enabled=true"
  flags+=" -Des.logs_stream_feature_flag_enabled=true -Des.slice_indexing_feature_flag_enabled=true"
  ES_ARGS=(-e discovery.type=single-node -e "xpack.license.self_generated.type=$1"
    -e ES_JAVA_OPTS="-Xms${ES_HEAP:-768m} -Xmx${ES_HEAP:-768m} $flags${ES_JVM_EXTRA:+ $ES_JVM_EXTRA}"
    -e node.attr.testattr=test -e path.repo=/tmp -e "repositories.url.allowed_urls=http://snapshot.*"
    -e action.destructive_requires_name=false -e script.disable_max_compilations_rate=true
    -e indices.breaker.total.use_real_memory=false -e cluster.deprecation_indexing.enabled=false
    -e cluster.routing.allocation.disk.watermark.low=1b -e cluster.routing.allocation.disk.watermark.high=1b
    -e cluster.routing.allocation.disk.watermark.flood_stage=1b -e node.processors=2
    -e "reindex.remote.whitelist=127.0.0.1:*,172.*:*" -e indices.lifecycle.history_index_enabled=false
    -e ingest.geoip.downloader.enabled=false)
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
  es_test_cluster_args trial
  docker run --rm -d --name "$name" -m "${ES_MEMORY:-1536m}" "${security[@]}" "${ES_ARGS[@]}" \
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
# More search-dev arguments (-E settings of the extended suites' clusters).
ROLL_EXTRA_ARGS=()
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
      # A port something already answers on belongs to another process: until ours fails to
      # bind, the readiness check below would take that server for ours (and run tests that
      # wipe its cluster).
      if curl -fs -m 2 "http://127.0.0.1:$port/" >/dev/null 2>&1; then
        port=$((port + 1))
        continue
      fi
      # The server is ours once its own log (one per run) says it listens: another run may
      # bind the port between the check above and ours.
      local log=$OUT/runs/search-dev-$port.$$.log
      "$bin" --listen "127.0.0.1:$port" --dialect "$dialect" --yaml-test-cluster "${ROLL_EXTRA_ARGS[@]}" >"$log" 2>&1 &
      local pid=$!
      for _ in $(seq 1 100); do
        if grep -q "listening" "$log" && kill -0 "$pid" 2>/dev/null && curl -fs "http://127.0.0.1:$port/" >/dev/null 2>&1; then
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
    local extra=
    [[ ${#ROLL_EXTRA_ARGS[@]} -gt 0 ]] && extra=" $(printf '%q ' "${ROLL_EXTRA_ARGS[@]}")"
    ROLL_URLS=(--spawn "'$dev' --listen 127.0.0.1:{port} --dialect ${dialect%-sql} --yaml-test-cluster$extra" --jobs "${ROLL_JOBS:-4}")
  else
    trap stop_roll EXIT
    start_roll "${dialect%-sql}" "$dev"
  fi
  local status=0
  if [[ $dialect == elasticsearch ]]; then
    fetch_es
    "$target/search-yaml" run "${ROLL_URLS[@]}" --api-spec "$ES_DIR/rest-api-spec/api" \
      --suite "free=$ES_DIR/rest-api-spec/test/free" --suite "clients=$ROOT/crates/search-yaml/suites/clients-tests/tests" \
      --skip-list "$base/es-$ES_VERSION.skips.txt" --baseline "$base/es-$ES_VERSION.txt" \
      --report "$OUT/roll.json" --quiet "$@" || status=$?
  elif [[ $dialect == opensearch-sql ]]; then
    fetch_opensearch
    fetch_opensearch_sql
    "$target/search-yaml" run "${ROLL_URLS[@]}" --api-spec "$OS_DIR/api" --api-spec "$OS_SQL_DIR/api" \
      --suite "sql=$OS_SQL_DIR/test" --baseline "$base/opensearch-$OS_VERSION-sql.txt" \
      --report "$OUT/roll-opensearch-sql.json" --quiet "$@" || status=$?
  else
    fetch_opensearch
    "$target/search-yaml" run "${ROLL_URLS[@]}" --api-spec "$OS_DIR/api" --suite "os=$OS_DIR/test" \
      --skip-list "$base/opensearch-$OS_VERSION.skips.txt" --baseline "$base/opensearch-$OS_VERSION.txt" \
      --report "$OUT/roll-opensearch.json" --quiet "$@" || status=$?
  fi
  stop_roll
  return $status
}

# ---------------------------------------------------------------------------------------------
# Extended suites: Elasticsearch's module, plugin and x-pack YAML suites (crates/search-yaml/suites/
# extended.txt), each run on the cluster its Gradle build configures.

EXT_SRC=$OUT/es-$ES_VERSION-src
EXT_LIST=$ROOT/crates/search-yaml/suites/extended.txt
EXT_OUT=$OUT/extended
# The user every secured test cluster defines (ESRestTestCase's default login).
EXT_REST_USER=x_pack_rest_user:x-pack-test-password
# The role of a test cluster user defined without one (test-clusters User.ROOT_USER_ROLE): every
# privilege, restricted indices included (which superuser lacks), from default_test_roles.yml.
EXT_ROOT_ROLE=_es_test_root

# The yamlRestTest resources of every module, plugin and x-pack plugin of the v$ES_VERSION tag (plus
# the few files their clusters load) in target/search-yaml/es-$ES_VERSION-src, from a blobless sparse
# clone (about 10 MB: no other file's content is downloaded). Not Apache-licensed: never committed.
fetch_extended() {
  fetch_es
  if [[ -f $EXT_SRC/.complete ]]; then
    echo "search-yaml: Elasticsearch $ES_VERSION module and x-pack suites already in $EXT_SRC"
    return
  fi
  local tmp=$OUT/.fetch-extended
  rm -rf "$tmp"
  mkdir -p "$tmp"
  echo "search-yaml: fetching the Elasticsearch v$ES_VERSION module and x-pack YAML suites (sparse clone)"
  git -c advice.detachedHead=false clone -q --depth 1 --filter=blob:none --no-checkout --branch "v$ES_VERSION" \
    https://github.com/elastic/elasticsearch.git "$tmp/es"
  git -C "$tmp/es" sparse-checkout set --no-cone \
    '/modules/*/src/yamlRestTest/resources/' '/plugins/*/src/yamlRestTest/resources/' \
    '/x-pack/plugin/*/src/yamlRestTest/resources/' '/x-pack/plugin/*/qa/*/src/yamlRestTest/resources/' \
    '/x-pack/plugin/security/src/test/resources/org/elasticsearch/xpack/security/authc/service/service_tokens' \
    '/test/fixtures/geoip-fixture/src/main/resources/geoip-fixture/GeoLite2-*.mmdb' \
    '/test/test-clusters/src/main/resources/default_test_roles.yml' '/LICENSE.txt'
  git -C "$tmp/es" checkout -q
  git -C "$tmp/es" rev-parse HEAD >"$tmp/es/COMMIT"
  rm -rf "$tmp/es/.git" "$EXT_SRC"
  mv "$tmp/es" "$EXT_SRC"
  rm -rf "$tmp"
  touch "$EXT_SRC/.complete"
  echo "search-yaml: Elasticsearch $ES_VERSION module and x-pack suites in $EXT_SRC ($(find "$EXT_SRC" -name '*.yml' -path '*/rest-api-spec/test/*' | wc -l | tr -d ' ') test files)"
}

# The lines of extended.txt (comments dropped), only those of the suites named in the arguments
# when there are any.
ext_lines() {
  local line name found
  for name in "$@"; do
    found=$(grep -E "^$name[[:space:]]" "$EXT_LIST" || true)
    [[ -n $found ]] || { echo "search-yaml: no extended suite named $name (see $EXT_LIST)" >&2; exit 2; }
  done
  while read -r line; do
    [[ -z $line || $line == \#* ]] && continue
    name=${line%%[[:space:]]*}
    if [[ $# -eq 0 || " $* " == *" $name "* ]]; then echo "$line"; fi
  done <"$EXT_LIST"
}

# ext_suite <name> <source>: sets EXT_ROOT (the suite's test directory) and EXT_RE (the regex of
# its test ids, which keeps only the selected subdirectories).
ext_suite() {
  local name=$1 src=$2 sel= dir dirs=()
  if [[ $src == *:*:* || ($src != zip:* && $src == *:*) ]]; then
    sel=${src##*:}
    src=${src%:*}
  fi
  if [[ $src == zip:* ]]; then
    EXT_ROOT=$ES_DIR/rest-api-spec/${src#zip:}
  else
    EXT_ROOT=$EXT_SRC/$src/src/yamlRestTest/resources/rest-api-spec/test
  fi
  [[ -d $EXT_ROOT ]] || { echo "search-yaml: $name: no tests in $EXT_ROOT (run fetch-extended)" >&2; exit 2; }
  if [[ -z $sel ]]; then
    EXT_RE="^$name:"
    return
  fi
  if [[ $sel == -* ]]; then
    for dir in "$EXT_ROOT"/*/; do
      dir=$(basename "$dir")
      [[ ",${sel#-}," == *",$dir,"* ]] || dirs+=("$dir")
    done
  else
    IFS=, read -r -a dirs <<<"$sel"
  fi
  local alt
  alt=$(printf '%s|' "${dirs[@]}" | sed 's/[.]/\\./g')
  EXT_RE="^$name:(${alt%|})/"
}

# The groups of suites that share a cluster configuration (profile and options), one per line:
# "<profile> <options...>\t<suite> <suite>...". Order of first appearance.
ext_groups() {
  ext_lines "$@" | awk '{
      key = $2; for (i = 4; i <= NF; i++) key = key " " $i
      if (!(key in seen)) { seen[key] = 1; order[++n] = key }
      names[key] = names[key] " " $1
    } END { for (i = 1; i <= n; i++) printf "%s\t%s\n", order[i], substr(names[order[i]], 2) }'
}

# ext_config <profile> <options...>: the cluster configuration of a group in globals
# (EXT_PROFILE, EXT_LICENSE, EXT_SECURITY, EXT_SETTINGS, EXT_ROLL_SETTINGS, EXT_JVM, EXT_USERS, EXT_LOGIN, EXT_ADMIN, EXT_ROLES,
# EXT_CONFIGS, EXT_RUNNER, EXT_MEMORY, EXT_HEAP).
ext_config() {
  EXT_PROFILE=$1
  shift
  EXT_LICENSE=trial EXT_SECURITY= EXT_JVM= EXT_USERS=$EXT_REST_USER:$EXT_ROOT_ROLE
  EXT_LOGIN=$EXT_REST_USER EXT_ADMIN= EXT_ROLES= EXT_MEMORY=1536m EXT_HEAP=768m
  EXT_SETTINGS=()
  EXT_ROLL_SETTINGS=()
  EXT_CONFIGS=()
  EXT_RUNNER=()
  case $EXT_PROFILE in
    plain | plugins) ;;
    basic) EXT_LICENSE=basic ;;
    security) EXT_SECURITY=1 ;;
    security-basic) EXT_SECURITY=1 EXT_LICENSE=basic ;;
    *) echo "search-yaml: unknown profile $EXT_PROFILE in $EXT_LIST" >&2; exit 2 ;;
  esac
  local opt
  for opt in "$@"; do
    case $opt in
      setting=*) EXT_SETTINGS+=("${opt#setting=}") ;;
      roll=*) EXT_ROLL_SETTINGS+=("${opt#roll=}") ;;
      jvm=*) EXT_JVM+="${EXT_JVM:+ }${opt#jvm=}" ;;
      users=*) EXT_USERS+=",${opt#users=}" ;;
      login=*) EXT_LOGIN=${opt#login=} ;;
      admin=*) EXT_ADMIN=${opt#admin=} ;;
      roles=*) EXT_ROLES=${opt#roles=} ;;
      config=*) EXT_CONFIGS+=("${opt#config=}") ;;
      runner=*) EXT_RUNNER+=("${opt#runner=}") ;;
      memory=*) EXT_MEMORY=${opt#memory=} ;;
      heap=*) EXT_HEAP=${opt#heap=} ;;
      *) echo "search-yaml: unknown option $opt in $EXT_LIST" >&2; exit 2 ;;
    esac
  done
  # A user the suite defines replaces the default one of the same name.
  EXT_USERS=$(tr , '\n' <<<"$EXT_USERS" | tac | awk -F: '!seen[$1]++' | tac | paste -sd, -)
}

# ext_roles_file <name>: the file realm's roles.yml of the current group in $EXT_OUT/<name>, as the
# test clusters write it: their default roles plus the suite's roles file. Containers bind-mount
# it, so every group gets a file of its own.
ext_roles_file() {
  local f=$EXT_OUT/$1
  cat "$EXT_SRC/test/test-clusters/src/main/resources/default_test_roles.yml" >"$f"
  if [[ -n $EXT_ROLES ]]; then
    echo >>"$f"
    cat "$EXT_SRC/$EXT_ROLES" >>"$f"
  fi
  echo "$f"
}

# A file extended.txt names: suites/... is one of ours (crates/search-yaml/suites), anything else
# is in the downloaded source tree.
ext_file() {
  if [[ $1 == suites/* ]]; then echo "$ROOT/crates/search-yaml/$1"; else echo "$EXT_SRC/$1"; fi
}

# The default image plus the official plugins the `plugins` profile's suites test.
es_plugins_image() {
  local image=roll-es-plugins:$ES_VERSION plugins
  if ! docker image inspect "$image" >/dev/null 2>&1; then
    plugins=$(awk '!/^#/ && $2 == "plugins" { sub(/^plugins\//, "", $3); printf "%s ", $3 }' "$EXT_LIST")
    printf 'FROM docker.elastic.co/elasticsearch/elasticsearch:%s\nRUN bin/elasticsearch-plugin install --batch %s\n' \
      "$ES_VERSION" "$plugins" | docker build -q -t "$image" - >/dev/null
  fi
  echo "$image"
}

# start_es_ext <container> <port>: one Elasticsearch configured as the current group (ext_config).
start_es_ext() {
  local name=$1 port=$2 image=docker.elastic.co/elasticsearch/elasticsearch:$ES_VERSION
  local args=() auth=() kv cfg entry
  [[ $EXT_PROFILE == plugins ]] && image=roll-es-plugins:$ES_VERSION
  docker image inspect "$image" >/dev/null 2>&1 || docker pull -q "$image" >/dev/null
  ES_CONTAINER=$name stop_es
  if [[ -n $EXT_SECURITY ]]; then
    # Security over plain HTTP, as the Gradle test clusters run it; the bootstrap password of
    # elastic is the one those clusters put in the keystore.
    args+=(-e xpack.security.enabled=true -e ELASTIC_PASSWORD=x-pack-test-password
      -e xpack.security.http.ssl.enabled=false -e xpack.security.transport.ssl.enabled=false
      -e xpack.security.authc.token.enabled=true -e xpack.security.authc.api_key.enabled=true)
    auth=(-u elastic:x-pack-test-password)
  else
    args+=(-e xpack.security.enabled=false)
  fi
  for kv in "${EXT_SETTINGS[@]}"; do args+=(-e "$kv"); done
  for cfg in "${EXT_CONFIGS[@]}"; do
    args+=(-v "$(ext_file "${cfg#*:}"):/usr/share/elasticsearch/config/${cfg%%:*}:ro")
  done
  [[ -n $EXT_SECURITY ]] && args+=(-v "$EXT_OUT/es-$EXT_TAG.roles.yml:/usr/share/elasticsearch/config/roles.yml:ro")
  ES_HEAP=$EXT_HEAP ES_JVM_EXTRA=$EXT_JVM es_test_cluster_args "$EXT_LICENSE"
  docker run --rm -d --name "$name" -m "$EXT_MEMORY" "${ES_ARGS[@]}" "${args[@]}" \
    -p "127.0.0.1:$port:9200" "$image" >/dev/null
  local up=
  for _ in $(seq 1 240); do
    if curl -fs "${auth[@]}" "http://127.0.0.1:$port/_cluster/health?wait_for_status=yellow&timeout=1s" >/dev/null 2>&1; then
      up=1
      break
    fi
    sleep 1
  done
  if [[ -z $up ]]; then
    echo "search-yaml: Elasticsearch ($name) did not start on port $port" >&2
    docker logs --tail 50 "$name" >&2 || true
    return 1
  fi
  [[ -n $EXT_SECURITY ]] || return 0
  # File realm users (they survive the feature-state resets between tests). The realm reloads its
  # users and their roles every 5 s, so wait until every user logs in with its roles.
  local users
  IFS=, read -r -a users <<<"$EXT_USERS"
  for entry in "${users[@]}"; do
    local u=${entry%%:*} rest=${entry#*:}
    local p=${rest%%:*} roles=${rest#*:}
    local out
    out=$(docker exec "$name" bin/elasticsearch-users useradd "$u" -p "$p" -r "${roles//|/,}" 2>&1) ||
      { echo "search-yaml: Elasticsearch ($name): adding user $u failed: $out" >&2; return 1; }
  done
  # Every user, not only the logins: tests send other users' credentials in headers.
  for entry in "${users[@]}"; do
    local u=${entry%%:*} rest=${entry#*:}
    local p=${rest%%:*} roles=${rest#*:}
    for _ in $(seq 1 60); do
      curl -fs -u "$u:$p" "http://127.0.0.1:$port/_security/_authenticate" 2>/dev/null |
        grep -q "\"${roles%%[,|]*}\"" && continue 2
      sleep 1
    done
    echo "search-yaml: Elasticsearch ($name): user $u can't log in with its roles" >&2
    return 1
  done
}

# ES_JOBS clusters of the current group, in parallel; ES_URLS gets the runner's --url arguments.
start_es_ext_all() {
  local port=${ES_PORT:-19202} pids=() i
  ES_URLS=()
  ext_roles_file "es-$EXT_TAG.roles.yml" >/dev/null
  [[ $EXT_PROFILE == plugins ]] && es_plugins_image >/dev/null
  for i in $(seq 0 $((${ES_JOBS:-1} - 1))); do
    start_es_ext "$(es_name "$i")" $((port + i)) &
    pids+=($!)
    ES_URLS+=(--url "http://127.0.0.1:$((port + i))")
  done
  for i in "${pids[@]}"; do
    wait "$i" || { stop_es_all; exit 1; }
  done
}

# ext_runner_args <suite>...: the runner arguments of one group (suites, id filter, skip lists,
# login) in EXT_ARGS.
ext_runner_args() {
  local name re=
  EXT_ARGS=(--api-spec "$ES_DIR/rest-api-spec/api")
  for name in "$@"; do
    local src
    src=$(awk -v n="$name" '$1 == n { print $3 }' "$EXT_LIST")
    ext_suite "$name" "$src"
    EXT_ARGS+=(--suite "$name=$EXT_ROOT")
    # The module's own API specs, as the Java runner loads every spec on its classpath.
    [[ -d $EXT_ROOT/../api ]] && EXT_ARGS+=(--api-spec "$(cd "$EXT_ROOT/../api" && pwd)")
    re+="${re:+|}${EXT_RE#^}"
    [[ -f $ROOT/crates/search-yaml/baseline/es-$ES_VERSION-$name.skips.txt ]] &&
      EXT_ARGS+=(--skip-list "$ROOT/crates/search-yaml/baseline/es-$ES_VERSION-$name.skips.txt")
  done
  EXT_ARGS+=(--filter "^($re)" "${EXT_RUNNER[@]}")
  if [[ -n $EXT_SECURITY ]]; then
    EXT_ARGS+=(--user "${EXT_LOGIN%%:*}" --password "${EXT_LOGIN#*:}")
    [[ -z $EXT_ADMIN ]] || EXT_ARGS+=(--admin-user "${EXT_ADMIN%%:*}" --admin-password "${EXT_ADMIN#*:}")
  fi
}

# ext_split_args [suite...] [-- runner args...]: EXT_NAMES and EXT_RUNNER_ARGS.
ext_split_args() {
  EXT_NAMES=()
  EXT_RUNNER_ARGS=()
  while [[ $# -gt 0 ]]; do
    if [[ $1 == -- ]]; then
      shift
      EXT_RUNNER_ARGS=("$@")
      return
    fi
    EXT_NAMES+=("$1")
    shift
  done
}

# Runs the extended suites ($@: suite names, all when none; runner arguments after `--`) against
# private Elasticsearch clusters and writes crates/search-yaml/baseline/es-$ES_VERSION-<suite>.txt,
# one group of suites with the same cluster configuration at a time, on ES_JOBS clusters each
# (ES_JOBS defaults to 4 here, containers roll-es-ext[-i] from ES_PORT).
es_extended_baseline() {
  ext_split_args "$@"
  fetch_extended
  local base=$ROOT/crates/search-yaml/baseline groups key names g=0 name
  : "${ES_JOBS:=4}" "${ES_CONTAINER:=roll-es-ext}"
  export ES_JOBS ES_CONTAINER
  trap stop_es_all EXIT
  cargo build -q -p search-yaml
  local bin=${SEARCH_YAML:-${CARGO_TARGET_DIR:-$ROOT/target}/debug/search-yaml}
  mkdir -p "$EXT_OUT"
  groups=$(ext_groups "${EXT_NAMES[@]}")
  while IFS=$'\t' read -r -u 3 key names; do
    g=$((g + 1))
    # Reports are named after the group's first suite, so runs over other subsets don't clash.
    local tag=${names%% *}
    EXT_TAG=$tag
    # shellcheck disable=SC2086
    ext_config $key
    # shellcheck disable=SC2086
    ext_runner_args $names
    echo "search-yaml: cluster $g ($key): $names"
    start_es_ext_all
    run_for_baseline "$EXT_OUT/es-$tag.json" "$EXT_OUT/es-$tag.pass.txt" \
      "$bin" run "${ES_URLS[@]}" "${EXT_ARGS[@]}" --max-failures 100 --quiet "${EXT_RUNNER_ARGS[@]}" </dev/null
    stop_es_all
    for name in $names; do
      {
        echo "# Tests of Elasticsearch $ES_VERSION's \`$name\` YAML suite that pass on a private single-node"
        echo "# Elasticsearch $ES_VERSION in Docker configured like the suite's own test cluster ($key)."
        echo "# Written by scripts/search-yaml-suites.sh es-extended-baseline (crates/search-yaml/suites/extended.txt)."
        grep -E "^$name:" "$EXT_OUT/es-$tag.pass.txt" | LC_ALL=C sort || true
      } >"$base/es-$ES_VERSION-$name.txt"
      echo "search-yaml: wrote $base/es-$ES_VERSION-$name.txt ($(grep -vc '^#' "$base/es-$ES_VERSION-$name.txt") passing tests)"
    done
  done 3<<<"$groups"
}

# Checks extended.txt: every suite's tests and cluster files are there; prints the test count of
# each suite (by the runner's own parser).
ext_check() {
  fetch_extended
  cargo build -q -p search-yaml
  local bin=${SEARCH_YAML:-${CARGO_TARGET_DIR:-$ROOT/target}/debug/search-yaml}
  local line name profile src opts f total=0 n
  while read -r line; do
    read -r name profile src opts <<<"$line"
    ext_suite "$name" "$src"
    for f in $opts; do
      case $f in
        config=*) f=${f#config=}; f=${f#*:} ;;
        roles=*) f=${f#roles=} ;;
        *) continue ;;
      esac
      [[ -f $(ext_file "$f") ]] || { echo "search-yaml: $name: missing $(ext_file "$f")" >&2; exit 1; }
    done
    n=$("$bin" list --suite "$name=$EXT_ROOT" --filter "$EXT_RE" | wc -l)
    (( n > 0 )) || { echo "search-yaml: $name: no tests" >&2; exit 1; }
    printf '%-26s %-15s %5d\n' "$name" "$profile" "$n"
    total=$((total + n))
  done < <(ext_lines "$@")
  printf '%-26s %-15s %5d\n' total "" "$total"
}

# ext_roll_roles <search-yaml>: the group's file-realm roles.yml (ext_roles_file) as the JSON
# object roll reads from `roll.security.file_roles`; prints its path.
ext_roll_roles() {
  local yml json
  yml=$(ext_roles_file "roll-$EXT_TAG.roles.yml")
  json=${yml%.yml}.json
  "$1" dump "$yml" | python3 -I -c '
import json, sys
roles = {}
for line in sys.stdin:
    roles.update(json.loads(line) or {})
json.dump({name: role or {} for name, role in roles.items()}, open(sys.argv[1], "w"))
' "$json"
  echo "$json"
}

# Per-suite counts of the roll-extended reports: a table on stdout and in $OUT/roll-extended.tsv
# (suite, tests, ES passes, roll passes, roll passes of the ES ones, regressions).
ext_summary() {
  python3 -I - "$OUT/roll-extended.tsv" "$ROOT/crates/search-yaml/baseline" "$ES_VERSION" "$@" <<'EOF'
import json, os, sys
out, base, version, reports = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4:]
suites = {}
for r in reports:
    for t in json.load(open(r))["tests"]:
        s = suites.setdefault(t["suite"], {"tests": 0, "pass": set(), "skip": 0})
        s["tests"] += 1
        if t["status"] == "pass":
            s["pass"].add(t["id"])
        elif t["status"] == "skip":
            s["skip"] += 1
rows = []
for name in sorted(suites):
    s = suites[name]
    path = os.path.join(base, f"es-{version}-{name}.txt")
    es = set()
    if os.path.exists(path):
        # Ids keep their trailing spaces (a few tests' names end in one).
        es = {l.rstrip("\r\n") for l in open(path) if l.strip() and not l.startswith("#")}
    rows.append((name, s["tests"], len(es), len(s["pass"]), len(s["pass"] & es), len(es - s["pass"])))
with open(out, "w") as f:
    f.write("suite\ttests\tes\troll\troll_of_es\tregressions\n")
    for r in rows:
        f.write("\t".join(map(str, r)) + "\n")
print(f"{'suite':26} {'tests':>6} {'ES':>6} {'roll':>6} {'of ES':>6} {'missing':>8}")
tot = [0] * 5
for r in rows:
    print(f"{r[0]:26} {r[1]:6} {r[2]:6} {r[3]:6} {r[4]:6} {r[5]:8}")
    tot = [a + b for a, b in zip(tot, r[1:])]
print(f"{'total':26} {tot[0]:6} {tot[1]:6} {tot[2]:6} {tot[3]:6} {tot[4]:8}")
EOF
}

# The extended suites ($@: suite names, all when none; runner arguments after `--`) against
# search-dev, one group at a time ($ROLL_JOBS processes started with the group's settings and
# users), compared with their baselines. Exit status 1 when a suite passes fewer of its baseline
# tests than Elasticsearch (which scripts/test-search.sh tolerates: it checks minimum pass counts).
roll_extended() {
  ext_split_args "$@"
  fetch_extended
  mkdir -p "$OUT/runs" "$EXT_OUT"
  local target=${CARGO_TARGET_DIR:-$ROOT/target}/debug
  if [[ -n ${SEARCH_DEV:-} ]]; then
    cargo build -q -p search-yaml
  else
    cargo build -q -p search-yaml -p search-server --bin search-yaml --bin search-dev
  fi
  local dev=${SEARCH_DEV:-$target/search-dev} bin=${SEARCH_YAML:-$target/search-yaml}
  local groups key members g=0 status=0 reports=() kv
  groups=$(ext_groups "${EXT_NAMES[@]}")
  trap stop_roll EXIT
  while IFS=$'\t' read -r -u 3 key members; do
    g=$((g + 1))
    local tag=${members%% *}
    # shellcheck disable=SC2086
    ext_config $key
    # shellcheck disable=SC2086
    ext_runner_args $members
    # The test clusters' root role is a superuser on roll; the group's roles.yml becomes roll's
    # file-realm roles (ext_roll_roles).
    ROLL_EXTRA_ARGS=(-E "roll.security.file_users=${EXT_USERS//:$EXT_ROOT_ROLE/:superuser}"
      -E "xpack.license.self_generated.type=$EXT_LICENSE")
    [[ -n $EXT_SECURITY ]] && ROLL_EXTRA_ARGS+=(-E xpack.security.enabled=true
      -E "roll.security.file_roles=$(EXT_TAG=$tag ext_roll_roles "$bin")")
    for kv in "${EXT_SETTINGS[@]}"; do
      case $kv in
        node.name=*) ROLL_EXTRA_ARGS+=(--node-name "${kv#node.name=}") ;;
        *) ROLL_EXTRA_ARGS+=(-E "$kv") ;;
      esac
    done
    for kv in "${EXT_ROLL_SETTINGS[@]}"; do ROLL_EXTRA_ARGS+=(-E "$kv"); done
    # The suite's config files (custom user agent regexes, …) go to a directory search-dev reads
    # as its `path.conf`.
    if (( ${#EXT_CONFIGS[@]} )); then
      local conf=$EXT_OUT/roll-$tag.config cfg
      rm -rf "$conf"
      for cfg in "${EXT_CONFIGS[@]}"; do
        mkdir -p "$(dirname "$conf/${cfg%%:*}")"
        cp "$(ext_file "${cfg#*:}")" "$conf/${cfg%%:*}"
      done
      ROLL_EXTRA_ARGS+=(-E "path.conf=$conf")
    fi
    local baseline=$EXT_OUT/roll-$tag.baseline.txt name
    : >"$baseline"
    for name in $members; do
      cat "$ROOT/crates/search-yaml/baseline/es-$ES_VERSION-$name.txt" >>"$baseline" 2>/dev/null || true
    done
    echo "search-yaml: roll group $g ($key): $members"
    start_roll elasticsearch "$dev"
    local rc=0
    rm -f "$EXT_OUT/roll-$tag.json"
    "$bin" run "${ROLL_URLS[@]}" "${EXT_ARGS[@]}" --baseline "$baseline" \
      --report "$EXT_OUT/roll-$tag.json" --quiet "${EXT_RUNNER_ARGS[@]}" </dev/null || rc=$?
    stop_roll
    [[ -f $EXT_OUT/roll-$tag.json ]] || { echo "search-yaml: roll group $g wrote no report" >&2; exit 2; }
    (( rc > status )) && status=$rc
    reports+=("$EXT_OUT/roll-$tag.json")
  done 3<<<"$groups"
  ROLL_EXTRA_ARGS=()
  ext_summary "${reports[@]}"
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
  roll-os-sql) roll_run opensearch-sql "$@" ;;
  fetch-extended) fetch_extended ;;
  es-extended-baseline) es_extended_baseline "$@" ;;
  roll-extended) roll_extended "$@" ;;
  extended-list) ext_lines "$@" ;;
  extended-check) ext_check "$@" ;;
  *) echo "usage: $0 [fetch|fetch-es|fetch-opensearch|es-baseline|es-start|es-stop|os-baseline|roll|roll-os|roll-os-sql|fetch-extended|es-extended-baseline|roll-extended|extended-list|extended-check|vendor-clients-tests [ref]]" >&2; exit 2 ;;
esac
