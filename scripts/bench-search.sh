#!/usr/bin/env bash
# roll search benchmarks against Elasticsearch and OpenSearch on the same CPUs and memory
# (docs/benchmarks/search.md).
#
#   scripts/bench-search.sh start | stop       start (stop) both targets
#   scripts/bench-search.sh micro [args...]    the in-repo micro suite (scripts/bench-search.py)
#                                              against both, interleaved per workload
#   scripts/bench-search.sh ingest --file <docs.json> --body <index.json> [args...]
#                                              bulk indexing at 1/8/32 clients against both
#                                              (`bench-search.py ingest`)
#   scripts/bench-search.sh rally <track> [rounds]
#                                              Elastic's Rally (elastic/rally image, benchmark-only
#                                              pipeline), alternating roll and Elasticsearch per
#                                              round; summary of medians and spread
#   scripts/bench-search.sh footprint          RSS idle and with a loaded cluster, cold start
#                                              (roll: wake after idle unload; Elasticsearch: node
#                                              start), store bytes per document
#
# Both targets run in Docker with the same --cpuset-cpus and -m (BENCH_CPUS, default 10-13,28-31:
# 4 cores / 8 hardware threads of the 18-core host;
# BENCH_MEM, default 4g); roll is search-dev (--data-dir, so it persists like Elasticsearch) in a
# plain Debian image, Elasticsearch is the official image with a 2 GiB heap, security off. Only one
# target is ever under load at a time: runs alternate, so drift on a shared host (load changes
# over minutes) hits both alike.
#
# OpenSearch (the official image, 2 GiB heap, security plugin off) is a third target unless
# BENCH_OS=0; runs rotate through all of them.
#
# Env: SEARCH_DEV (a search-dev binary; default: a release build in target/bench-search),
#      ROLL_PORT (19801), ES_PORT (19210), ES_VERSION (9.5.5), ES_NAME (roll-search-es-ref),
#      OS_PORT (19211), OS_VERSION (3.9.0), OS_NAME (roll-search-os-ref), BENCH_OS (1),
#      ROLL_NAME (roll-bench-roll), RALLY_CPUS (6-9,24-27), RALLY_HOME (~/.cache/roll-search/rally:
#      tracks and corpora, about 300 MB for geonames), RALLY_PARAMS (extra track params),
#      RALLY_TASKS / RALLY_EXCLUDE (Rally --include-tasks / --exclude-tasks filters),
#      RALLY_TARGETS (which targets race, default all: "roll es os"),
#      RALLY_INDICES (the indices the track creates, default <track>*; http_logs: logs-*,reindexed-logs),
#      RALLY_DOCS / RALLY_CORPORA (index only the first N documents of the large corpora, see
#      `bench-search.py rally-subset`),
#      OUT (target/bench-search).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

CPUS=${BENCH_CPUS:-10-13,28-31}
MEM=${BENCH_MEM:-4g}
ROLL_PORT=${ROLL_PORT:-19801}
ES_PORT=${ES_PORT:-19210}
ES_VERSION=${ES_VERSION:-9.5.5}
ES_NAME=${ES_NAME:-roll-search-es-ref}
OS_PORT=${OS_PORT:-19211}
OS_VERSION=${OS_VERSION:-3.9.0}
OS_NAME=${OS_NAME:-roll-search-os-ref}
BENCH_OS=${BENCH_OS:-1}
ROLL_NAME=${ROLL_NAME:-roll-bench-roll}
ROLL_IMAGE=${ROLL_IMAGE:-debian:trixie-slim}
RALLY_CPUS=${RALLY_CPUS:-6-9,24-27}
RALLY_HOME=${RALLY_HOME:-$HOME/.cache/roll-search/rally}
OUT=${OUT:-$ROOT/target/bench-search}
mkdir -p "$OUT"

docker_() {
  if docker info >/dev/null 2>&1; then docker "$@"; else sg docker -c "$(printf '%q ' docker "$@")"; fi
}

search_dev() {
  if [[ -n ${SEARCH_DEV:-} ]]; then
    echo "$SEARCH_DEV"
    return
  fi
  local dir=$ROOT/target/bench-search/cargo
  CARGO_TARGET_DIR=$dir cargo build --release -q -p search-server --bin search-dev >&2
  echo "$dir/release/search-dev"
}

wait_http() { # url seconds
  local i
  for ((i = 0; i < $2 * 10; i++)); do
    curl -sf "$1" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "bench-search: $1 did not answer within $2 s" >&2
  return 1
}

running() { [[ -n $(docker_ ps -q -f "name=^$1\$") ]]; }

start_es() {
  if ! running "$ES_NAME"; then
    docker_ run --rm -d --name "$ES_NAME" --cpuset-cpus "$CPUS" -m "$MEM" \
      -e discovery.type=single-node -e xpack.security.enabled=false \
      -e xpack.license.self_generated.type=trial -e "ES_JAVA_OPTS=-Xms2g -Xmx2g" \
      -p "127.0.0.1:$ES_PORT:9200" "docker.elastic.co/elasticsearch/elasticsearch:$ES_VERSION" >/dev/null
  fi
  wait_http "http://127.0.0.1:$ES_PORT/_cluster/health?wait_for_status=yellow" 180
}

start_os() {
  [[ $BENCH_OS == 1 ]] || return 0
  if ! running "$OS_NAME"; then
    docker_ run --rm -d --name "$OS_NAME" --cpuset-cpus "$CPUS" -m "$MEM" \
      -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true -e DISABLE_INSTALL_DEMO_CONFIG=true \
      -e "OPENSEARCH_JAVA_OPTS=-Xms2g -Xmx2g" \
      -p "127.0.0.1:$OS_PORT:9200" "opensearchproject/opensearch:$OS_VERSION" >/dev/null
  fi
  wait_http "http://127.0.0.1:$OS_PORT/_cluster/health?wait_for_status=yellow" 180
}

# The targets other than roll: name, port and container, one per line.
others() {
  echo "es $ES_PORT $ES_NAME"
  [[ $BENCH_OS == 1 ]] && echo "os $OS_PORT $OS_NAME"
  true
}

# --target / --cgroup arguments of bench-search.py for every target.
target_args() {
  local name port ctr
  printf '%s\n' --target "roll=http://127.0.0.1:$ROLL_PORT" $(cgroup_of "$ROLL_NAME" roll)
  while read -r name port ctr; do
    printf '%s\n' --target "$name=http://127.0.0.1:$port" $(cgroup_of "$ctr" "$name")
  done < <(others)
}

# start_roll [extra search-dev args...]: a fresh roll node with an empty data directory.
start_roll() {
  local bin
  bin=$(search_dev)
  running "$ROLL_NAME" && docker_ stop -t 5 "$ROLL_NAME" >/dev/null
  rm -rf "$OUT/roll-data" && mkdir -p "$OUT/roll-data"
  docker_ run --rm -d --name "$ROLL_NAME" --cpuset-cpus "$CPUS" -m "$MEM" --network host \
    --user "$(id -u):$(id -g)" -e RUST_LOG=warn \
    -v "$bin:/usr/local/bin/search-dev:ro" -v "$OUT/roll-data:/data" "$ROLL_IMAGE" \
    search-dev --listen "127.0.0.1:$ROLL_PORT" --data-dir /data "$@" >/dev/null
  wait_http "http://127.0.0.1:$ROLL_PORT/" 30
}

stop_all() {
  running "$ROLL_NAME" && docker_ stop -t 5 "$ROLL_NAME" >/dev/null
  running "$ES_NAME" && docker_ stop -t 30 "$ES_NAME" >/dev/null
  running "$OS_NAME" && docker_ stop -t 30 "$OS_NAME" >/dev/null
  true
}

# RSS (KiB) of the container's main process (search-dev, or Elasticsearch's JVM): the largest
# VmRSS among the processes of its cgroup (`docker top -o` fails on some Docker versions).
rss_kib() {
  local id d p m=0 r
  id=$(docker_ inspect -f '{{.Id}}' "$1")
  for d in "/sys/fs/cgroup/system.slice/docker-$id.scope" "/sys/fs/cgroup/docker/$id"; do
    [[ -r $d/cgroup.procs ]] || continue
    for p in $(cat "$d/cgroup.procs"); do
      r=$(awk '/^VmRSS:/ { print $2 }' "/proc/$p/status" 2>/dev/null || true)
      [[ -n $r ]] && ((r > m)) && m=$r
    done
    break
  done
  echo "$m"
}

# The container's cgroup v2 directory (its cpu.stat gives the server CPU time), if visible.
cgroup_of() {
  local id d
  id=$(docker_ inspect -f '{{.Id}}' "$1")
  for d in "/sys/fs/cgroup/system.slice/docker-$id.scope" "/sys/fs/cgroup/docker/$id"; do
    [[ -r $d/cpu.stat ]] && echo "--cgroup=$2=$d" && return
  done
}

# local_track <track>: the standard track with the bulk tasks' warmup cut to $RALLY_BULK_WARMUP
# seconds (default 10). The tracks warm indexing up for 70-120 s, longer than a whole subset
# corpus takes to index here, which leaves Rally no indexing throughput to report. Prints the
# --track-path argument; the corpora stay in the shared data directory.
local_track() {
  local src=$RALLY_HOME/benchmarks/tracks/default/$1 dst=$RALLY_HOME/local-tracks/$1
  if [[ ! -d $src ]]; then
    # First use: let Rally fetch the track repository.
    docker_ run --rm --network host --user "$(id -u):$(id -g)" -v "$RALLY_HOME:/rally/.rally" \
      elastic/rally:latest info --track="$1" >/dev/null
  fi
  rm -rf "$dst" && mkdir -p "$RALLY_HOME/local-tracks" && cp -r "$src" "$dst"
  find "$dst" -name '*.json' -exec sed -i -E \
    "s/\"warmup-time-period\": *[0-9]+/\"warmup-time-period\": ${RALLY_BULK_WARMUP:-10}/" {} +
  # No target throughput, so service times are what each side can do: geonames and nyc_taxis
  # take include_target_throughput:false (see below); the others (http_logs) hard-code their
  # rates, sized for the full corpus, so they are dropped here (with the comma before them: the
  # key is never first in its object).
  if ! grep -rq include_target_throughput "$dst"; then
    find "$dst" -name '*.json' -exec perl -0pi -e 's/,\s*"target-throughput": *[0-9.]+//g' {} +
  fi
  # OpenSearch has no index.mapping.source.mode: nyc_taxis sets it to its default ("stored", also
  # Elasticsearch's), so without a source_mode parameter the line becomes another default both
  # accept (a line, not a deletion: the setting before it ends in a comma).
  if [[ ${RALLY_PARAMS:-} != *source_mode* ]]; then
    find "$dst" -name '*.json' -exec perl -pi -e \
      's/"index\.mapping\.source\.mode": *\{\{ *source_mode *\| *default\("stored"\) *\| *tojson *\}\}/"index.max_result_window": 10000/' {} +
  fi
  # RALLY_DOCS: index only the first N documents of each larger corpus (RALLY_CORPORA: a regex on
  # <corpus>/<source-file> limiting which ones); streamed once into the data directory.
  if [[ -n ${RALLY_DOCS:-} ]]; then
    python3 "$ROOT/scripts/bench-search.py" rally-subset "$dst" "$RALLY_HOME/benchmarks/data" \
      "$RALLY_DOCS" "${RALLY_CORPORA:-}" >&2
  fi
  echo "--track-path=/rally/.rally/local-tracks/$1"
}

# drop_es_indices: deletes Elasticsearch's indices matching $indices by name (a wildcard
# DELETE is refused: action.destructive_requires_name defaults to true).
drop_es_indices() { # [port]
  local names port=${1:-$ES_PORT}
  names=$(curl -sf "http://127.0.0.1:$port/_cat/indices/$indices?h=index&ignore_unavailable=true" | tr -s ' \n' ',' | sed 's/^,//; s/,$//') || true
  [[ -n $names ]] && curl -sf -XDELETE "http://127.0.0.1:$port/$names" >/dev/null || true
}

rally_race() { # name port track params report
  mkdir -p "$RALLY_HOME/out"
  # Rally refuses a server that isn't Elasticsearch (its client's product check) or reports a
  # version below the track's minimum (OpenSearch says 3.9.0); OpenSearch runs the same tracks
  # with both checks skipped (no-ops for Elasticsearch and roll, which pass them).
  local esrally=/rally/venv/lib/python3.13/site-packages/esrally
  local unchecked='s/if self._verified_elasticsearch is not True:/if self._verified_elasticsearch is not True and False:/'
  local unversioned='s/if specified_version < min_es_version:/if False:/'
  docker_ run --rm --network host --cpuset-cpus "$RALLY_CPUS" -m 4g --user "$(id -u):$(id -g)" \
    -v "$RALLY_HOME:/rally/.rally" --entrypoint sh elastic/rally:latest \
    -c "sed -i '$unchecked' $esrally/client/synchronous.py && sed -i '$unversioned' $esrally/racecontrol.py && exec /entrypoint.sh \"\$@\"" sh \
    race "$TRACK_ARG" --pipeline=benchmark-only \
    --target-hosts="127.0.0.1:$2" --track-params="$4" --kill-running-processes \
    ${RALLY_TASKS:+--include-tasks="$RALLY_TASKS"} ${RALLY_EXCLUDE:+--exclude-tasks="$RALLY_EXCLUDE"} \
    --client-options="timeout:600" --on-error=continue --user-tags="target:$1" --report-format=csv \
    --report-file="/rally/.rally/out/$5" >"$OUT/$5.log" 2>&1 || {
      echo "bench-search: rally failed for $1 (see $OUT/$5.log)" >&2
      tail -20 "$OUT/$5.log" >&2
      return 1
    }
  cp "$RALLY_HOME/out/$5" "$OUT/$5"
}

case ${1:-} in
  start)
    start_es
    start_os
    start_roll
    echo "roll http://127.0.0.1:$ROLL_PORT  Elasticsearch http://127.0.0.1:$ES_PORT  OpenSearch http://127.0.0.1:$OS_PORT"
    ;;
  stop) stop_all ;;
  micro)
    shift
    start_es
    start_os
    start_roll
    mapfile -t targets < <(target_args)
    python3 scripts/bench-search.py "${targets[@]}" --out "$OUT/micro.json" "$@"
    ;;
  ingest)
    shift
    start_es
    start_os
    start_roll
    mapfile -t targets < <(target_args)
    python3 scripts/bench-search.py ingest "${targets[@]}" "$@"
    ;;
  rally)
    track=${2:?track}
    rounds=${3:-3}
    # Full corpus unless RALLY_PARAMS says otherwise (geonames: ingest_percentage:10 for 1.1M
    # docs, http_logs: ingest_percentage:2 for 4.9M); no target throughput (local_track).
    params="number_of_replicas:0${RALLY_PARAMS:+,$RALLY_PARAMS}"
    # The indices the track creates (dropped from Elasticsearch between races; http_logs: logs-*).
    indices=${RALLY_INDICES:-$track*}
    start_es
    start_os
    TRACK_ARG=$(local_track "$track")
    if grep -rq include_target_throughput "$RALLY_HOME/local-tracks/$track"; then
      params+=",include_target_throughput:false"
    fi
    stamp=$(date +%Y%m%d-%H%M%S)
    for ((r = 0; r < rounds; r++)); do
      # Rotate who goes first, so no target always runs in the same part of the hour.
      all=(${RALLY_TARGETS:-roll $(others | cut -d' ' -f1)})
      order=("${all[@]:r % ${#all[@]}}" "${all[@]:0:r % ${#all[@]}}")
      for t in "${order[@]}"; do
        case $t in
          roll) start_roll; port=$ROLL_PORT ctr=$ROLL_NAME ;;
          es) port=$ES_PORT ctr=$ES_NAME; drop_es_indices "$port" ;;
          os) port=$OS_PORT ctr=$OS_NAME; drop_es_indices "$port" ;;
        esac
        echo "bench-search: round $r $t $track" >&2
        rally_race "$t" "$port" "$track" "$params" "rally-$track-$stamp-$t-$r.csv"
        echo "$t $r rss_kib $(rss_kib "$ctr")" >>"$OUT/rally-$track-$stamp.rss"
        [[ $t == roll ]] || drop_es_indices "$port"
      done
    done
    python3 scripts/bench-search.py rally-summary "$OUT"/rally-"$track"-"$stamp"-*.csv
    ;;
  footprint)
    start_es
    start_os
    start_roll --idle-unload-ms 2000
    sleep 2
    # The other targets: name, port, container (OpenSearch unless BENCH_OS=0).
    mapfile -t rest < <(others)
    hdr="| measure | roll |" sep="|---|---:|"
    for o in "${rest[@]}"; do read -r n _ _ <<<"$o"; hdr+=" $n |"; sep+="---:|"; done
    echo "$hdr"; echo "$sep"
    row() { # label roll-value then one value per other target from the function $2
      local line="| $1 | $2 |" o n p c
      shift 2
      for o in "${rest[@]}"; do read -r n p c <<<"$o"; line+=" $("$@" "$n" "$p" "$c") |"; done
      echo "$line"
    }
    rss_mib() { echo $(($(rss_kib "$3") / 1024)); }
    row "RSS, idle node (MiB)" "$(($(rss_kib "$ROLL_NAME") / 1024))" rss_mib
    targets=(--target "roll=http://127.0.0.1:$ROLL_PORT")
    for o in "${rest[@]}"; do read -r n p _ <<<"$o"; targets+=(--target "$n=http://127.0.0.1:$p"); done
    python3 scripts/bench-search.py "${targets[@]}" --rounds 1 --only bulk,match --reps 50 --keep >/dev/null
    for u in "http://127.0.0.1:$ROLL_PORT" $(for o in "${rest[@]}"; do read -r _ p _ <<<"$o"; echo "http://127.0.0.1:$p"; done); do
      curl -sf -XPOST "$u/perf-bench/_forcemerge?max_num_segments=1" >/dev/null
      curl -sf -XPOST "$u/perf-bench/_flush" >/dev/null
    done
    row "RSS, one loaded 50k-doc index (MiB)" "$(($(rss_kib "$ROLL_NAME") / 1024))" rss_mib
    store() { curl -sf "$1/_cat/indices/perf-bench?h=store.size&bytes=b" | tr -d ' \n'; }
    store_per_doc() { echo $(($(store "http://127.0.0.1:$2") / 50000)); }
    row "store bytes per document (force-merged)" "$(($(store "http://127.0.0.1:$ROLL_PORT") / 50000))" store_per_doc
    # roll: unload after 2 s idle, then time the first request (the wake) and the second.
    q='{"query":{"match":{"body":"w7"}}}'
    wakes=() hots=()
    for i in 1 2 3 4 5; do
      sleep 4
      t0=$(date +%s%N)
      curl -sf -H 'Content-Type: application/json' -d "$q" "http://127.0.0.1:$ROLL_PORT/perf-bench/_search" >/dev/null
      t1=$(date +%s%N)
      curl -sf -H 'Content-Type: application/json' -d "$q" "http://127.0.0.1:$ROLL_PORT/perf-bench/_search" >/dev/null
      t2=$(date +%s%N)
      wakes+=($(((t1 - t0) / 1000)))
      hots+=($(((t2 - t1) / 1000)))
    done
    rss_unloaded=$(($(rss_kib "$ROLL_NAME") / 1024))
    med() { printf '%s\n' "$@" | sort -n | sed -n "$((($# + 1) / 2))p"; }
    # Elasticsearch and OpenSearch have no unload: their cold start is a node start (JVM,
    # cluster state, shard recovery) until the first search answers.
    restart_ms() {
      docker_ restart -t 30 "$3" >/dev/null
      local t0
      t0=$(date +%s%N)
      wait_http "http://127.0.0.1:$2/perf-bench/_search?q=body:w7" 300
      echo "$((($(date +%s%N) - t0) / 1000000)) (node restart)"
    }
    wake="$(($(med "${wakes[@]}") / 1000)).$(printf '%03d' $(($(med "${wakes[@]}") % 1000))) (wake after unload; warm $(($(med "${hots[@]}") / 1000)).$(printf '%03d' $(($(med "${hots[@]}") % 1000))))"
    row "cold start to first search hit (ms)" "$wake" restart_ms
    echo "| RSS after idle unload (MiB) | $rss_unloaded |"
    for o in "${rest[@]}"; do read -r _ p _ <<<"$o"; curl -sf -XDELETE "http://127.0.0.1:$p/perf-bench" >/dev/null || true; done
    ;;
  *)
    sed -n '2,25p' "$0"
    exit 2
    ;;
esac
