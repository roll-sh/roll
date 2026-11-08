#!/bin/sh
# Benchmark harness: the engine (over the wire and in-process) against PostgreSQL 18.6, both
# pinned to the same CPUs. See BENCH.md.
#
#   tools/bench/run.sh pg-start | pg-stop | engine-start [--no-sync] | engine-stop
#   tools/bench/run.sh pgrust-start | pgrust-stop
#   tools/bench/run.sh pgdef-start | pgdef-stop | engperf-start | engperf-stop
#   tools/bench/run.sh pgbench <pg|engine|pgrust|pgdef|engperf> <pgbench args...>
#   tools/bench/run.sh bench <pg|engine> <workload> [--opt value...]
#   tools/bench/run.sh inproc <workload> [--opt value...]   (the engine server stopped)
#   tools/bench/run.sh psql <pg|engine> <sql>
#   tools/bench/run.sh du <pg|engine>
#   tools/bench/run.sh mem <pg|engine>
#
# Without the docker group in this shell (a session older than the membership) it runs itself
# again under `sg docker`.
set -eu
if [ -z "${BENCH_SG:-}" ] && ! docker info >/dev/null 2>&1; then
  q=""; for a in "$0" "$@"; do q="$q '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"; done
  BENCH_SG=1 exec sg docker -c "BENCH_SG=1 sh $q"
fi
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
DATA=${BENCH_DATA:-$HOME/.cache/a36}
SERVER_CPUS=${SERVER_CPUS:-28-31}
CLIENT_CPUS=${CLIENT_CPUS:-32-35}
PG_PORT=${PG_PORT:-55360}
ENGINE_PORT=${ENGINE_PORT:-55361}
PG_NAME=${PG_NAME:-a36-pg}
PG_IMAGE=${PG_IMAGE:-postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873}
# ENGINE_DOCKER=1 runs the engine in a container as PostgreSQL's runs (system.slice, the same
# cpuset and CPU weight): a host process sits in the user slice, which every agent's builds
# share, while a container's slice gets its own weight, so a host engine competes for CPU on
# worse terms than PostgreSQL. The image only supplies glibc (the binary is bind-mounted).
# ENGINE_ENV="VAR=value ..." sets the engine's environment (in the container or on the host),
# e.g. ROLL_MODEL_MAX_VERSIONS for tools/bench/sizing.
ENGINE_NAME=${ENGINE_NAME:-$PG_NAME-engine}
ENGINE_IMAGE=${ENGINE_IMAGE:-debian@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad}
BIN=${BENCH_BIN:-$ROOT/target/release}
BENCH=${BENCH_CLIENT:-$BIN/examples/bench}; [ -x "$BENCH" ] || BENCH=$BIN/bench
mkdir -p "$DATA/logs"

# pgrust (github.com/malisper/pgrust, AGPL: benchmarked only), a third server for the
# scoreboard: its image, run as PostgreSQL's is, on PGRUST_PORT.
PGRUST_NAME=${PGRUST_NAME:-$PG_NAME-pgrust}
PGRUST_PORT=${PGRUST_PORT:-55362}
PGRUST_IMAGE=${PGRUST_IMAGE:-malisper/pgrust@sha256:9ec8cbd68b422729ef3d5f6062e8ac501607357a0a5832bd1c6e0b69670360a9}
# Two more servers for the modes' comparison: pgdef, PostgreSQL with its stock settings (pg is
# tuned: shared_buffers=2GB and the others below), and engperf, the engine in performance mode
# (`--mode performance`, its caches sized from the container's memory; engine is the engine as
# given: efficient mode unless ENGINE_ARGS says otherwise, --cache-mb CACHE_MB).
PGDEF_NAME=${PGDEF_NAME:-$PG_NAME-def}
PGDEF_PORT=${PGDEF_PORT:-55363}
ENGPERF_NAME=${ENGPERF_NAME:-$PG_NAME-engperf}
ENGPERF_PORT=${ENGPERF_PORT:-55364}
# SERVER_MEM (e.g. 8g) caps every server container's memory (docker --memory): performance mode
# sizes its caches from it, as PostgreSQL's tuning guidance sizes shared_buffers (a quarter).
MEMOPT=${SERVER_MEM:+--memory $SERVER_MEM}
port() {
  case $1 in pg) echo "$PG_PORT" ;; pgrust) echo "$PGRUST_PORT" ;; pgdef) echo "$PGDEF_PORT" ;;
    engperf) echo "$ENGPERF_PORT" ;; *) echo "$ENGINE_PORT" ;; esac
}
cname() {
  case $1 in pg) echo "$PG_NAME" ;; pgrust) echo "$PGRUST_NAME" ;; pgdef) echo "$PGDEF_NAME" ;;
    engperf) echo "$ENGPERF_NAME" ;; *) echo "$ENGINE_NAME" ;; esac
}
is_engine() { [ "$1" = engine ] || [ "$1" = engperf ]; }
memopt() {
  if ! is_engine "$1" || [ -n "${ENGINE_DOCKER:-}" ]; then
    id=$(docker inspect -f '{{.Id}}' "$(cname "$1")")
    echo "--mem-cgroup /sys/fs/cgroup/system.slice/docker-$id.scope"
  else
    echo "--mem-pid $(cat "$DATA/$1.pid")"
  fi
}
# pg_start <name> <port> <volume> <settings...>: a PostgreSQL container.
pg_start() {
  n=$1; p=$2; shift 2
  # PG_NETWORK=host serves on the host network (no docker-proxy between client and server: the
  # engine has none); PG_CPU_SHARES lowers its CPU weight (2 is about nice 19's, for runs at
  # low priority).
  if [ "${PG_NETWORK:-}" = host ]; then
    net="--network host"; listen="-c port=$p -c listen_addresses=127.0.0.1"; inport=$p
  else
    net="-p 127.0.0.1:$p:5432"; listen=""; inport=5432
  fi
  # shellcheck disable=SC2086
  docker run -d --name "$n" --cpuset-cpus "$SERVER_CPUS" --shm-size=1g $MEMOPT \
    ${PG_CPU_SHARES:+--cpu-shares "$PG_CPU_SHARES"} $net -v "$n-data":/var/lib/postgresql \
    -e POSTGRES_PASSWORD=pw -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" \
    "$PG_IMAGE" "$@" $listen >/dev/null
  i=0; until docker exec "$n" pg_isready -U postgres -h 127.0.0.1 -p "$inport" -q 2>/dev/null; do sleep 1; i=$((i+1)); [ $i -lt 60 ]; done
  sleep 2
  docker exec "$n" psql -U postgres -p "$inport" -Atc "select version()"
}
# engine_start <engine|engperf> <roll-engine args...>: data in $DATA/<t>, pid in $DATA/<t>.pid.
engine_start() {
  t=$1; shift; n=$(cname "$t"); p=$(port "$t"); d=$DATA/$t
  if ss -ltn | grep -q ":$p "; then echo "port $p busy" >&2; exit 1; fi
  mkdir -p "$d"
  if [ -n "${ENGINE_DOCKER:-}" ]; then
    docker rm -f "$n" >/dev/null 2>&1 || true
    # shellcheck disable=SC2086
    docker run -d --name "$n" --cpuset-cpus "$SERVER_CPUS" $MEMOPT \
      ${PG_CPU_SHARES:+--cpu-shares "$PG_CPU_SHARES"} --network host --user "$(id -u):$(id -g)" \
      -v "$BIN:$BIN:ro" -v "$d:$d" -v /usr/share/zoneinfo:/usr/share/zoneinfo:ro \
      -e TMPDIR="$d" $(for e in ${ENGINE_ENV:-}; do printf ' -e %s' "$e"; done) \
      "$ENGINE_IMAGE" "$BIN/roll-engine" --data "$d" \
      --listen 127.0.0.1:$p --password pw "$@" >/dev/null
    # (The container's process, as the host sees it: /proc probes work as for a host engine.)
    docker inspect -f '{{.State.Pid}}' "$n" >"$DATA/$t.pid"
  else
    env ${ENGINE_ENV:-} taskset -c "$SERVER_CPUS" "$BIN/roll-engine" --data "$d" --listen 127.0.0.1:$p \
      --password pw "$@" >>"$DATA/logs/$t.log" 2>&1 &
    echo $! >"$DATA/$t.pid"
  fi
  i=0; until ss -ltn | grep -q ":$p "; do sleep 0.2; i=$((i+1)); [ $i -lt 300 ]; done
  echo "$t pid $(cat "$DATA/$t.pid")"
}
engine_stop() {
  t=$1; n=$(cname "$t")
  if [ -n "${ENGINE_DOCKER:-}" ]; then
    if docker inspect "$n" >/dev/null 2>&1; then
      docker logs "$n" >>"$DATA/logs/$t.log" 2>&1 || true
      docker stop -t 60 "$n" >/dev/null 2>&1 || true
      docker rm -f "$n" >/dev/null 2>&1 || true
    fi
    rm -f "$DATA/$t.pid"
  elif [ -f "$DATA/$t.pid" ]; then
    kill "$(cat "$DATA/$t.pid")" 2>/dev/null || true
    while kill -0 "$(cat "$DATA/$t.pid")" 2>/dev/null; do sleep 0.2; done
    rm -f "$DATA/$t.pid"
  fi
}

cmd=$1; shift
case $cmd in
pg-start)
  # Plan-affecting settings (work_mem, effective_cache_size, random_page_cost) stay at their
  # defaults, as the engine has them, so both run the same plans.
  pg_start "$PG_NAME" "$PG_PORT" -c shared_buffers=2GB -c maintenance_work_mem=256MB -c max_wal_size=8GB \
    -c checkpoint_timeout=30min -c max_connections=300
  ;;
pgdef-start)
  # PostgreSQL as installed: its stock settings, but for max_connections (the footprint gate's
  # 100 idle sessions and the 100-session workload need more than the stock 100's 97).
  docker rm -f "$PGDEF_NAME" >/dev/null 2>&1 || true
  docker volume rm "$PGDEF_NAME-data" >/dev/null 2>&1 || true
  pg_start "$PGDEF_NAME" "$PGDEF_PORT" -c max_connections=300
  ;;
pgdef-stop)
  docker rm -f "$PGDEF_NAME" >/dev/null 2>&1 || true
  docker volume rm "$PGDEF_NAME-data" >/dev/null 2>&1 || true
  ;;
pg-stop)
  docker rm -f "$PG_NAME" >/dev/null 2>&1 || true
  docker volume rm "$PG_NAME-data" >/dev/null 2>&1 || true
  ;;
pgrust-start)
  # Its defaults, plus what its README asks of a server (a 64 MB stack, RUST_MIN_STACK,
  # max_stack_depth, io_method=sync) and max_connections for the footprint gate's 100 idle
  # sessions (its default 100 leaves 97). The entrypoint is the official image's: the port is
  # PGPORT's (a `-c port` would hide the entrypoint's temporary server from it).
  docker rm -f "$PGRUST_NAME" >/dev/null 2>&1 || true
  docker volume rm "$PGRUST_NAME-data" >/dev/null 2>&1 || true
  # shellcheck disable=SC2086
  docker run -d --name "$PGRUST_NAME" --cpuset-cpus "$SERVER_CPUS" --shm-size=1g $MEMOPT \
    ${PG_CPU_SHARES:+--cpu-shares "$PG_CPU_SHARES"} --network host --ulimit stack=67108864 \
    -e RUST_MIN_STACK=33554432 -e PGPORT="$PGRUST_PORT" -v "$PGRUST_NAME-data":/var/lib/postgresql \
    -e POSTGRES_PASSWORD=pw -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" \
    "$PGRUST_IMAGE" -c listen_addresses=127.0.0.1 -c max_stack_depth=60000 -c io_method=sync \
    -c max_connections=300 >/dev/null
  i=0
  until docker exec "$PGRUST_NAME" psql -U postgres -h 127.0.0.1 -p "$PGRUST_PORT" -Atc "select 1" >/dev/null 2>&1; do
    sleep 1; i=$((i+1)); [ $i -lt 180 ] || exit 1
  done
  # (The entrypoint's temporary server answers too: wait for the one that stays.)
  sleep 5
  until docker exec "$PGRUST_NAME" psql -U postgres -h 127.0.0.1 -p "$PGRUST_PORT" -Atc "select 1" >/dev/null 2>&1; do
    sleep 1; i=$((i+1)); [ $i -lt 180 ] || exit 1
  done
  docker exec "$PGRUST_NAME" psql -U postgres -h 127.0.0.1 -p "$PGRUST_PORT" -Atc "select version()"
  ;;
pgrust-stop)
  docker rm -f "$PGRUST_NAME" >/dev/null 2>&1 || true
  docker volume rm "$PGRUST_NAME-data" >/dev/null 2>&1 || true
  ;;
engine-start)
  # (CACHE_MB= set empty: no --cache-mb, the mode's own size.)
  c=${CACHE_MB-2048}
  engine_start engine ${c:+--cache-mb "$c"} ${ENGINE_ARGS:-} "$@"
  ;;
engine-stop)
  engine_stop engine
  ;;
engperf-start)
  # Performance mode with its own sizing (from SERVER_MEM's limit, else the machine's memory).
  engine_start engperf --mode performance ${ENGPERF_ARGS:-} "$@"
  ;;
engperf-stop)
  engine_stop engperf
  ;;
pgbench)
  t=$1; shift
  docker run --rm --network host --cpuset-cpus "$CLIENT_CPUS" -e PGPASSWORD=pw "$PG_IMAGE" \
    pgbench -h 127.0.0.1 -p "$(port "$t")" -U postgres "$@"
  ;;
bench)
  t=$1; shift
  # shellcheck disable=SC2046
  taskset -c "$CLIENT_CPUS" "$BENCH" --target wire --label "$t" --port "$(port "$t")" \
    $(memopt "$t") "$@"
  ;;
inproc)
  # The client and the engine share a process: give it both CPU sets, as the wire runs have.
  taskset -c "$SERVER_CPUS,$CLIENT_CPUS" "$BENCH" --target inproc --label inproc \
    --data "$DATA/engine" --cache-mb "${CACHE_MB:-2048}" "$@"
  ;;
psql)
  t=$1; shift
  docker run --rm --network host -e PGPASSWORD=pw "$PG_IMAGE" \
    psql -h 127.0.0.1 -p "$(port "$t")" -U postgres -d "${BENCH_DB:-bench}" -Atc "$1"
  ;;
du)
  if [ "$1" = pg ]; then
    docker exec "$PG_NAME" sh -c 'du -sm $PGDATA $PGDATA/base $PGDATA/pg_wal'
  else
    du -sm "$DATA/engine"; du -sm "$DATA/engine"/* 2>/dev/null | sort -n | tail -3
  fi
  ;;
mem)
  if [ "$1" = pg ]; then
    id=$(docker inspect -f '{{.Id}}' "$PG_NAME")
    grep -E '^(anon|shmem|file) ' "/sys/fs/cgroup/system.slice/docker-$id.scope/memory.stat"
  else
    grep -E 'VmRSS|VmHWM' "/proc/$(cat "$DATA/engine.pid")/status"
  fi
  ;;
cpu)
  # The engine's CPU use over $2 seconds (in % of one core).
  p=$(cat "$DATA/engine.pid"); s=${2:-2}
  a=$(awk '{print $14+$15}' "/proc/$p/stat"); sleep "$s"; b=$(awk '{print $14+$15}' "/proc/$p/stat")
  echo "engine cpu: $(( (b - a) * 100 / $(getconf CLK_TCK) / s ))%"
  ;;
*) echo "unknown command $cmd" >&2; exit 2 ;;
esac
