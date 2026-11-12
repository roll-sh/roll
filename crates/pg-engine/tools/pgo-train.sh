#!/bin/sh
# The training run of a profile-guided build (tools/dist-pgo.sh runs it; HANDOFF "Profile-guided
# build"): every workload class we benchmark, against an instrumented roll-engine, so that the
# profile it writes favours none of them over another.
#
#   tools/pgo-train.sh <instrumented roll-engine> <bench client> <raw profile dir>
#
# The engine runs on the host with LLVM_PROFILE_FILE=<raw dir>/engine-%p-%m.profraw and stops
# with SIGTERM (a smart shutdown runs the exit handlers that write the profile). The clients are
# the bench example (tools/bench/bench.rs, any build) and psql / pgbench from the PostgreSQL
# image the benchmarks use, on the host network. All data is generated here. Environment:
#   PGO_PORT         the engine's port (default 55440; it must be free)
#   PGO_WORK         scratch directory on disk for the data directory (default <raw dir>/../work)
#   PGO_SERVER_CPUS  taskset CPUs of the engine, PGO_CLIENT_CPUS of the clients (default: none)
#   PGO_TRAIN_SKIP   classes left out, space-separated (startup point range short bulk tpch
#                    catalog regress): only for a hold-out measurement, never for a shipped build
# Time: about 8 minutes on 2 cores for the engine. Classes and their shares are set below; a
# class's OLTP parts run for fixed seconds, the others for fixed work.
set -eu
# Without the docker group in this shell (a session older than the membership), again under sg.
if [ -z "${PGO_SG:-}" ] && ! docker info >/dev/null 2>&1; then
  q=""; for a in "$0" "$@"; do q="$q '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"; done
  PGO_SG=1 exec sg docker -c "PGO_SG=1 sh $q"
fi
ENGINE=$(realpath "${1:?usage: pgo-train.sh <instrumented roll-engine> <bench client> <raw dir>}")
BENCH=$(realpath "${2:?bench client}")
RAW=$(mkdir -p "${3:?raw dir}" && realpath "$3")
HERE=$(cd "$(dirname "$0")" && pwd)
SQL=$HERE/../tests/sql
PORT=${PGO_PORT:-55440}
WORK=${PGO_WORK:-$(dirname "$RAW")/work}
SKIP=" ${PGO_TRAIN_SKIP:-} "
PGIMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
CLI=pgo-train-cli-$$
DATA=$WORK/data
ts() { [ -n "${PGO_SERVER_CPUS:-}" ] && echo "taskset -c $PGO_SERVER_CPUS" || true; }
tc() { [ -n "${PGO_CLIENT_CPUS:-}" ] && echo "taskset -c $PGO_CLIENT_CPUS" || true; }
want() { case $SKIP in *" $1 "*) echo "== skip $1"; return 1 ;; esac; echo "== $1 $(date +%T)"; }
log() { echo "   $*"; }

if ss -ltn | grep -q ":$PORT "; then echo "pgo-train: port $PORT busy" >&2; exit 1; fi
rm -rf "$WORK"; mkdir -p "$WORK/out" "$DATA"
rm -f "$RAW"/*.profraw
EPID=
start() {
  LLVM_PROFILE_FILE="$RAW/engine-%p-%m.profraw" TMPDIR="$WORK" $(ts) "$ENGINE" --data "$DATA" \
    --listen 127.0.0.1:"$PORT" --password pw --max-prepared-transactions 5 \
    >>"$WORK/engine.log" 2>&1 &
  EPID=$!
  i=0; until ss -ltn | grep -q ":$PORT "; do
    sleep 0.2; i=$((i + 1))
    kill -0 "$EPID" 2>/dev/null || { echo "pgo-train: the engine exited" >&2; tail -20 "$WORK/engine.log" >&2; exit 1; }
    [ $i -lt 600 ] || { echo "pgo-train: the engine did not listen" >&2; exit 1; }
  done
}
stop() {
  [ -n "$EPID" ] || return 0
  kill -TERM "$EPID" 2>/dev/null || true
  wait "$EPID" 2>/dev/null || true
  EPID=
}
cleanup() { stop; docker rm -f "$CLI" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

b() { $(tc) "$BENCH" --target wire --port "$PORT" --password pw --label train "$@" >>"$WORK/out/bench.log" 2>&1 \
  || { echo "pgo-train: bench $* failed" >&2; tail -20 "$WORK/out/bench.log" >&2; exit 1; }; }
psql() { timeout 300 docker exec -e PGPASSWORD=pw "$CLI" psql -X -q -h 127.0.0.1 -p "$PORT" -U postgres "$@"; }
pgbench() { docker exec -e PGPASSWORD=pw -w /w "$CLI" pgbench -h 127.0.0.1 -p "$PORT" -U postgres "$@" >>"$WORK/out/pgbench.log" 2>&1 \
  || { echo "pgo-train: pgbench $* failed" >&2; tail -20 "$WORK/out/pgbench.log" >&2; exit 1; }; }

# The client container: psql and pgbench of PostgreSQL 18.6, the golden SQL files mounted.
mkdir -p "$WORK/w"
printf '\\set a random(1, 100000)\nDELETE FROM events WHERE account = :a;\n' >"$WORK/w/delete.sql"
docker run -d --name "$CLI" --network host --user "$(id -u):$(id -g)" ${PGO_CLIENT_CPUS:+--cpuset-cpus $PGO_CLIENT_CPUS} \
  -v "$WORK/w:/w" -v "$(realpath "$SQL"):/sql:ro" --entrypoint sleep "$PGIMG" infinity >/dev/null

T0=$(date +%s)
# Startup: a new data directory (bootstrap), first sessions, a restart that recovers it.
start
b createdb
if want startup; then
  b startup; b startup-catalog
  stop; start; b startup-catalog
fi
b oltp-load --rows 100000

# Point reads: extended protocol with binary results (the bench client), simple protocol
# (text), extended with text parameters and results (pgbench -M extended), prepared text.
if want point; then
  b oltp --rows 100000 --clients 8 --secs 25 --only point_select
  b oltp --rows 100000 --clients 8 --secs 15 --only point_select_simple
  b oltp --rows 100000 --clients 1 --secs 5 --only point_select,select_1
  pgbench -i -I dtGvp -s 1 bench
  pgbench -n -S -M extended -c 8 -j 4 -T 10 bench
  pgbench -n -S -M prepared -c 8 -j 4 -T 10 bench
  pgbench -n -S -M simple -c 8 -j 4 -T 5 bench
fi
# Range reads: 100-row index ranges, a secondary index's aggregate.
if want range; then
  b oltp --rows 100000 --clients 8 --secs 25 --only range_scan_100
  b oltp --rows 100000 --clients 8 --secs 5 --only secondary_lookup
fi
# Short write transactions: the bench client's UPDATE / SELECT / INSERT transaction at 8, 32
# and 1 clients, pgbench's tpcb-like, single-row INSERT, UPDATE, upsert and DELETE.
if want short; then
  b oltp-load --rows 100000
  b oltp --rows 100000 --clients 8 --secs 25 --only short_txn
  b oltp --rows 100000 --clients 32 --secs 15 --only short_txn
  b oltp --rows 100000 --clients 1 --secs 5 --only short_txn
  b oltp --rows 100000 --clients 8 --secs 5 --only insert
  b oltp --rows 100000 --clients 8 --secs 5 --only update_indexed
  b oltp --rows 100000 --clients 8 --secs 5 --only upsert
  pgbench -n -f /w/delete.sql -c 8 -j 4 -T 5 bench
  [ -n "$(psql -d bench -Atc "SELECT 1 FROM pg_class WHERE relname = 'pgbench_accounts'")" ] || pgbench -i -I dtGvp -s 1 bench
  pgbench -n -M prepared -c 8 -j 4 -T 15 bench
  pgbench -n -M simple -c 8 -j 4 -T 5 bench
  psql -d bench -c CHECKPOINT
fi
# A restart over a store with data in it (recovery, the catalog's reload).
stop; start

# Bulk: COPY in and out, index builds, INSERT ... SELECT, UPDATE, DELETE, VACUUM, ANALYZE.
if want bulk; then
  psql -d bench -c "ALTER DATABASE bench SET maintenance_work_mem = '256MB'" >/dev/null
  b bulk --rows 1000000 --reps 1
fi
# TPC-H at a small scale: the load (COPY, indexes, VACUUM ANALYZE), all 22 queries, parallel.
if want tpch; then
  b tpch-load --sf 0.2
  b tpch --reps 2
fi
# Catalog: DDL as a schema grows, psql's \d family and ORM introspection (catalog-tools),
# psql's own queries.
if want catalog; then
  b scale --tables 1500 --schemas 20 --parts 100 --step 500 --reps 2 --keep 1
  b catalog-tools --tables 1500 --schemas 20 --reps 3
  for c in '\d' '\dt+' '\di' '\df' '\dn' '\l' '\du' '\d+ accounts' '\d events' '\d+ lineitem' '\dv' '\dT'; do
    psql -d bench -c "$c" >>"$WORK/out/meta.out" 2>&1 || log "psql $c failed"
  done
fi
# A slice of the golden SQL suite for breadth (every 25th file, each in a new database).
if want regress; then
  n=0
  for f in "$SQL"/*.sql; do
    n=$((n + 1)); [ $((n % 25)) = 0 ] || continue
    base=$(basename "$f" .sql)
    psql -d postgres -c "CREATE DATABASE t_$n" >/dev/null
    psql -d "t_$n" -f "/sql/$base.sql" >"$WORK/out/regress-$base.out" 2>&1 || true
    psql -d postgres -c "DROP DATABASE t_$n" >/dev/null 2>&1 || true
    log "regress $base"
  done
fi
stop
T1=$(date +%s)
n=$(ls "$RAW"/*.profraw 2>/dev/null | wc -l)
[ "$n" -gt 0 ] || { echo "pgo-train: no profile written (an uninstrumented engine?)" >&2; exit 1; }
echo "pgo-train: $((T1 - T0)) s, $n raw profiles, $(du -sh "$RAW" | cut -f1)"
rm -rf "$WORK"
