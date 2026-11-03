#!/bin/bash
# The size models' lift table (BENCH.md "Size models, round 4" and "round 5"): what modelling
# big tables costs, the engine (in a container, run.sh's ENGINE_DOCKER) against PostgreSQL 18.6
# on the same cores. Each check prints a LIFT line: server, label, check, CPU seconds of the
# server's cgroup, wall seconds, work (transactions or rows), peak anon + shmem MB.
#
#   tools/bench/sizing/lift.sh <label> <server: pg | engine> [checks...]
#
# checks (default: all, in this order):
#   bulk    pgbench -i -s $SCALE one phase at a time (COPY g, VACUUM v, primary key p), then
#           CREATE INDEX on bid, DELETE of every tenth row, VACUUM, and the first statement
#           after a clean restart (wall, psql's \timing)
#   pgbench select-only, simple-update and tpcb-like at $CLIENTS clients, $SECS s each, on a
#           fresh pgbench -i -s $SCALE
#   apply   the apply-like workload (apply.sql: agent 152's logical apply statements) on
#           extra_setup.sql's tables, $CLIENTS clients
#   skip    a warm skip scan, `count(*) FROM skip_t WHERE ts = K` (index (g, ts), 1M rows),
#           the median of 9 (wall, \timing)
#
# The engine's limit: ENGINE_ENV="ROLL_MODEL_MAX_VERSIONS=<n>" (1099511627776 lifts it).
# Knobs as run.sh's (BENCH_BIN: the dist build's directory, BENCH_DATA, SERVER_CPUS,
# CLIENT_CPUS, PG_PORT, ENGINE_PORT, PG_NAME, ENGINE_NAME) plus SCALE (10), CLIENTS (8),
# SECS (20). The server is started fresh and stopped at the end. liftcmp.sh runs the table.
set -uo pipefail
if [ -z "${BENCH_SG:-}" ] && ! docker info >/dev/null 2>&1; then
  q=""; for a in "$0" "$@"; do q="$q '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"; done
  BENCH_SG=1 exec sg docker -c "BENCH_SG=1 bash $q"
fi
LABEL=${1:?label}; T=${2:?pg or engine}; shift 2
CHECKS=${*:-bulk pgbench apply skip}
HERE=$(cd "$(dirname "$0")" && pwd)
RUN=$HERE/../run.sh
export PG_NETWORK=host PG_CPU_SHARES=${PG_CPU_SHARES:-2} ENGINE_DOCKER=${ENGINE_DOCKER-1}
export PG_NAME=${PG_NAME:-sizing-pg} BENCH_DATA=${BENCH_DATA:-$HOME/.cache/sizing}
export ENGINE_NAME=${ENGINE_NAME:-$PG_NAME-engine}
SCALE=${SCALE:-10}; CLIENTS=${CLIENTS:-8}; SECS=${SECS:-20}
IMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
port=${PG_PORT:-55360}; [ "$T" = engine ] && port=${ENGINE_PORT:-55361}
name=$PG_NAME; [ "$T" = engine ] && name=$ENGINE_NAME
mkdir -p "$BENCH_DATA/logs"; LOG=$BENCH_DATA/logs/lift-$LABEL-$T.log

cg() { echo "/sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' "$name").scope"; }
cpu() { awk '/^usage_usec/ {printf "%.3f", $2 / 1e6}' "$CG/cpu.stat"; }
mem() { awk '$1 == "anon" || $1 == "shmem" {s += $2} END {print s + 0}' "$CG/memory.stat"; }
sampler() { local m max=0; while [ ! -e "$1.stop" ]; do m=$(mem); [ "$m" -gt "$max" ] && max=$m; echo "$max" >"$1"; sleep 0.5; done; }
calc() { awk "BEGIN {printf \"%.3f\", $1}"; }
start() {
  if [ "$T" = pg ]; then "$RUN" pg-stop; "$RUN" pg-start >>"$LOG" 2>&1
  else "$RUN" engine-stop; rm -rf "$BENCH_DATA/engine"; "$RUN" engine-start >>"$LOG" 2>&1; fi
  CG=$(cg)
}
restart() { # a clean restart, the data kept
  if [ "$T" = pg ]; then docker restart -t 60 "$PG_NAME" >/dev/null
    until docker exec "$PG_NAME" pg_isready -U postgres -h 127.0.0.1 -p "$port" -q 2>/dev/null; do sleep 0.5; done
  else "$RUN" engine-stop; "$RUN" engine-start >>"$LOG" 2>&1; fi
  CG=$(cg)
}
stop() { if [ "$T" = pg ]; then "$RUN" pg-stop; else "$RUN" engine-stop; fi; }
trap stop EXIT
client() { # client <cmd...>: a pgbench / psql of the reference image on the host network
  docker run --rm -i --network host --cpuset-cpus "${CLIENT_CPUS:-32-35}" -e PGPASSWORD=pw \
    -v "$HERE:$HERE:ro" "$IMG" "$@"
}
sql() { client psql -X -q -At -v ON_ERROR_STOP=1 -h 127.0.0.1 -p "$port" -U postgres -d "${2:-bench}" -c "$1"; }
# measure <check> <command...>: the command's CPU, wall time and peak memory on the server
measure() {
  local check=$1 f=$BENCH_DATA/peak.$$ c0 c1 s0 s1 out sp work; shift
  rm -f "$f" "$f.stop"; echo 0 >"$f"; sampler "$f" & sp=$!
  c0=$(cpu); s0=$(date +%s.%N)
  out=$("$@" 2>&1); echo "$out" >>"$LOG"
  s1=$(date +%s.%N); c1=$(cpu); touch "$f.stop"; wait "$sp"
  work=$(echo "$out" | sed -n 's/^number of transactions actually processed: \([0-9]*\).*/\1/p' | head -1)
  printf 'LIFT\t%s\t%s\t%s\tcpu_s=%s\tsecs=%s\twork=%s\tpeak_mb=%s\n' "$T" "$LABEL" "$check" \
    "$(calc "$c1 - $c0")" "$(calc "$s1 - $s0")" "${work:-}" "$(calc "$(cat "$f") / 1048576")"
  rm -f "$f" "$f.stop"
}
# timed <check> <db> <sql>: the statement's own time, psql's \timing (ms), as a LIFT line
timed() {
  local ms; ms=$(printf '\\timing on\n%s\n' "$3" | client psql -X -At -h 127.0.0.1 -p "$port" -U postgres -d "$2" |
    sed -n 's/^Time: \([0-9.]*\) ms.*/\1/p' | sort -n | awk '{a[NR] = $1} END {print a[int((NR + 1) / 2)]}')
  printf 'LIFT\t%s\t%s\t%s\tms=%s\n' "$T" "$LABEL" "$1" "$ms"
}
pgb() { client pgbench -h 127.0.0.1 -p "$port" -U postgres "$@"; }

start
sql "CREATE DATABASE bench" postgres >/dev/null
for c in $CHECKS; do
  case $c in
  bulk)
    measure "init tables" pgb -i -I dt -s "$SCALE" bench
    measure "COPY (g)" pgb -i -I g -s "$SCALE" bench
    measure "VACUUM (v)" pgb -i -I v -s "$SCALE" bench
    measure "primary key (p)" pgb -i -I p -s "$SCALE" bench
    measure "CREATE INDEX (bid)" sql "CREATE INDEX pgbench_accounts_bid ON pgbench_accounts (bid)"
    measure "DELETE 10%" sql "DELETE FROM pgbench_accounts WHERE aid % 10 = 0"
    measure "VACUUM after DELETE" sql "VACUUM pgbench_accounts"
    restart
    timed "first statement after restart" bench "SELECT abalance FROM pgbench_accounts WHERE aid = 12345;"
    ;;
  pgbench)
    pgb -i -s "$SCALE" -q bench >>"$LOG" 2>&1
    for mode in select-only simple-update tpcb-like; do
      measure "pgbench $mode c$CLIENTS" pgb -n -M prepared -b "$mode" -c "$CLIENTS" -j "$(( CLIENTS < 4 ? CLIENTS : 4 ))" -T "$SECS" bench
    done
    ;;
  apply|skip)
    if ! sql "SELECT 1 FROM skip_t LIMIT 1" >/dev/null 2>&1; then
      client psql -X -q -v ON_ERROR_STOP=1 -h 127.0.0.1 -p "$port" -U postgres -d bench -f "$HERE/extra_setup.sql" >>"$LOG" 2>&1
    fi
    if [ "$c" = apply ]; then
      measure "apply-like c$CLIENTS" pgb -n -M prepared -f "$HERE/apply.sql" -c "$CLIENTS" -j "$(( CLIENTS < 4 ? CLIENTS : 4 ))" -T "$SECS" bench
    else
      q="SELECT count(*) FROM skip_t WHERE ts = 4321;"
      timed "skip scan warm" bench "$q$q$q$q$q$q$q$q$q$q"
    fi
    ;;
  *) echo "unknown check $c" >&2; exit 2 ;;
  esac
done
