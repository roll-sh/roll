#!/bin/sh
# Benchmark harness: the engine (over the wire and in-process) against PostgreSQL 18.6, both
# pinned to the same CPUs. See BENCH.md.
#
#   tools/bench/run.sh pg-start | pg-stop | engine-start [--no-sync] | engine-stop
#   tools/bench/run.sh pgbench <pg|engine> <pgbench args...>
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
PG_IMAGE=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
BIN=${BENCH_BIN:-$ROOT/target/release}
BENCH=${BENCH_CLIENT:-$BIN/examples/bench}; [ -x "$BENCH" ] || BENCH=$BIN/bench
mkdir -p "$DATA/logs"

port() { if [ "$1" = pg ]; then echo "$PG_PORT"; else echo "$ENGINE_PORT"; fi; }
memopt() {
  if [ "$1" = pg ]; then
    id=$(docker inspect -f '{{.Id}}' "$PG_NAME")
    echo "--mem-cgroup /sys/fs/cgroup/system.slice/docker-$id.scope"
  else
    echo "--mem-pid $(cat "$DATA/engine.pid")"
  fi
}

cmd=$1; shift
case $cmd in
pg-start)
  # Plan-affecting settings (work_mem, effective_cache_size, random_page_cost) stay at their
  # defaults, as the engine has them, so both run the same plans.
  docker run -d --name "$PG_NAME" --cpuset-cpus "$SERVER_CPUS" --shm-size=1g \
    -p 127.0.0.1:$PG_PORT:5432 -v "$PG_NAME-data":/var/lib/postgresql \
    -e POSTGRES_PASSWORD=pw -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" \
    "$PG_IMAGE" -c shared_buffers=2GB -c maintenance_work_mem=256MB -c max_wal_size=8GB \
    -c checkpoint_timeout=30min -c max_connections=300 >/dev/null
  i=0; until docker exec "$PG_NAME" pg_isready -U postgres -q 2>/dev/null; do sleep 1; i=$((i+1)); [ $i -lt 60 ]; done
  sleep 2
  docker exec "$PG_NAME" psql -U postgres -Atc "select version()"
  ;;
pg-stop)
  docker rm -f "$PG_NAME" >/dev/null 2>&1 || true
  docker volume rm "$PG_NAME-data" >/dev/null 2>&1 || true
  ;;
engine-start)
  if ss -ltn | grep -q ":$ENGINE_PORT "; then echo "port $ENGINE_PORT busy" >&2; exit 1; fi
  mkdir -p "$DATA/engine"
  taskset -c "$SERVER_CPUS" "$BIN/roll-engine" --data "$DATA/engine" --listen 127.0.0.1:$ENGINE_PORT \
    --password pw --cache-mb "${CACHE_MB:-2048}" "$@" >>"$DATA/logs/engine.log" 2>&1 &
  echo $! >"$DATA/engine.pid"
  i=0; until ss -ltn | grep -q ":$ENGINE_PORT "; do sleep 0.2; i=$((i+1)); [ $i -lt 300 ]; done
  echo "engine pid $(cat "$DATA/engine.pid")"
  ;;
engine-stop)
  if [ -f "$DATA/engine.pid" ]; then
    kill "$(cat "$DATA/engine.pid")" 2>/dev/null || true
    while kill -0 "$(cat "$DATA/engine.pid")" 2>/dev/null; do sleep 0.2; done
    rm -f "$DATA/engine.pid"
  fi
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
