#!/bin/sh
# The training run of a profile-guided build (tools/dist-pgo.sh runs it; HANDOFF "Profile-guided
# build"): every workload class we benchmark, against an instrumented roll-engine, so that the
# profile it writes favours none of them over another.
#
#   tools/pgo-train.sh <instrumented roll-engine> <bench client> <raw profile dir> [<breadth raw dir>]
#
# With a breadth raw dir, a second phase writes its own raw profiles there (dist-pgo.sh merges the
# two weighted, so that the benchmark classes keep most of the counts): every golden SQL file
# (tests/sql, four at a time, each in a new database) and PostgreSQL's regression suite
# (src/test/regress of REL_18_6, its parallel_schedule's groups four at a time; PGO_REGRESS_SRC
# names an existing copy of that directory, else a sparse clone is made next to the raw dirs).
# Code no training run reaches is compiled as cold, and was measured up to 13% slower than the
# plain dist build (HANDOFF "Profile-guided build"): the breadth phase is what keeps general SQL
# outside the benchmarks from that.
#
# The engine runs on the host with LLVM_PROFILE_FILE=<raw dir>/engine-%p-%m.profraw and stops
# with SIGTERM (a smart shutdown runs the exit handlers that write the profile). The clients are
# the bench example (tools/bench/bench.rs, any build) and psql / pgbench from the PostgreSQL
# image the benchmarks use, on the host network. All data is generated here. Environment:
#   PGO_PORT         the engine's port (default: one from tools/ports.sh, held while this runs)
#   PGO_WORK         scratch directory on disk for the data directory (default <raw dir>/../work)
#   PGO_SERVER_CPUS  taskset CPUs of the engine, PGO_CLIENT_CPUS of the clients (default: none)
#   PGO_TRAIN_SKIP   classes left out, space-separated (startup point range bulk short tpch
#                    catalog regress breadth): only for a hold-out measurement, never for a shipped
#                    build (tools/dist-pgo.sh refuses to merge a profile without every class
#                    unless PGO_TRAIN_SKIP is set, and marks such a build as not for shipping)
# Time: about 14 minutes on this machine (823-861 s, 2026-11-23). Classes and their shares are set below;
# a class's OLTP parts run for fixed seconds, the others for fixed work. A class that completes
# leaves <raw dir>/<class>/.ran; the engine must still be running at each class's end (an engine
# that died lost that run's counts: the script stops with an error).
set -eu
# Without the docker group in this shell (a session older than the membership), again under sg.
if [ -z "${PGO_SG:-}" ] && ! docker info >/dev/null 2>&1; then
  q=""; for a in "$0" "$@"; do q="$q '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"; done
  PGO_SG=1 exec sg docker -c "PGO_SG=1 sh $q"
fi
ENGINE=$(realpath "${1:?usage: pgo-train.sh <instrumented roll-engine> <bench client> <raw dir>}")
BENCH=$(realpath "${2:?bench client}")
RAW=$(mkdir -p "${3:?raw dir}" && realpath "$3")
BRAW=${4:-}; [ -z "$BRAW" ] || BRAW=$(mkdir -p "$BRAW" && realpath "$BRAW")
HERE=$(cd "$(dirname "$0")" && pwd)
SQL=$HERE/../tests/sql
PORT=${PGO_PORT:-$("$HERE/ports.sh" take $$ 1)}
WORK=${PGO_WORK:-$(dirname "$RAW")/work}
SKIP=" ${PGO_TRAIN_SKIP:-} "
PGIMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
CLI=pgo-train-cli-$$
DATA=$WORK/data
ts() { [ -n "${PGO_SERVER_CPUS:-}" ] && echo "taskset -c $PGO_SERVER_CPUS" || true; }
tc() { [ -n "${PGO_CLIENT_CPUS:-}" ] && echo "taskset -c $PGO_CLIENT_CPUS" || true; }
# (CUR: the class whose parts run; stop() marks it as ran once the engine wrote its counts.)
CUR=
want() { case $SKIP in *" $1 "*) echo "== skip $1"; return 1 ;; esac; echo "== $1 $(date +%T)"; CUR=$1; }
log() { echo "   $*"; }

if ss -ltn | grep -q ":$PORT "; then echo "pgo-train: port $PORT busy" >&2; exit 1; fi
rm -rf "$WORK"; mkdir -p "$WORK/out" "$DATA"
find "$RAW" -name '*.profraw' -delete
[ -z "$BRAW" ] || rm -f "$BRAW"/*.profraw
EPID=
PROFDIR=$RAW
start() {
  LLVM_PROFILE_FILE="$PROFDIR/engine-%p-%m.profraw" TMPDIR="$WORK" $(ts) "$ENGINE" --data "$DATA" \
    --listen 127.0.0.1:"$PORT" --password pw --max-prepared-transactions 5 --allow-server-files "$WORK" \
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
  # An engine that died before this lost its counts (they are written at exit).
  kill -TERM "$EPID" 2>/dev/null || { echo "pgo-train: the engine (pid $EPID) died during the training" >&2; tail -20 "$WORK/engine.log" >&2; EPID=; exit 1; }
  rc=0; wait "$EPID" 2>/dev/null || rc=$?
  EPID=
  [ $rc = 0 ] || { echo "pgo-train: the engine exited with status $rc at SIGTERM" >&2; tail -20 "$WORK/engine.log" >&2; exit 1; }
  [ -z "$CUR" ] || : >"$PROFDIR/.ran"
  CUR=
}
cleanup() { [ -z "$EPID" ] || kill -TERM "$EPID" 2>/dev/null || true; docker rm -f "$CLI" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

b() { $(tc) "$BENCH" --target wire --port "$PORT" --password pw --label train "$@" >>"$WORK/out/bench.log" 2>&1 \
  || { echo "pgo-train: bench $* failed" >&2; tail -20 "$WORK/out/bench.log" >&2; exit 1; }; }
psql() { timeout 300 docker exec -e PGPASSWORD=pw "$CLI" psql -X -q -h 127.0.0.1 -p "$PORT" -U postgres "$@"; }
pgbench() { docker exec -e PGPASSWORD=pw -w /w "$CLI" pgbench -h 127.0.0.1 -p "$PORT" -U postgres "$@" >>"$WORK/out/pgbench.log" 2>&1 \
  || { echo "pgo-train: pgbench $* failed" >&2; tail -20 "$WORK/out/pgbench.log" >&2; exit 1; }; }

# The client container: psql and pgbench of PostgreSQL 18.6, the golden SQL files mounted.
mkdir -p "$WORK/w"
printf '\\set a random(1, 100000)\nDELETE FROM events WHERE account = :a;\n' >"$WORK/w/delete.sql"
printf '\\set aid random(1, 100000 * :scale)\n\\set delta random(-5000, 5000)\nUPDATE pgbench_accounts SET abalance = abalance + :delta WHERE aid = :aid;\n' >"$WORK/w/update.sql"
docker run -d --name "$CLI" --network host --user "$(id -u):$(id -g)" ${PGO_CLIENT_CPUS:+--cpuset-cpus $PGO_CLIENT_CPUS} \
  -v "$WORK/w:/w" -v "$WORK:$WORK" -v "$(realpath "$SQL"):/sql:ro" --entrypoint sleep "$PGIMG" infinity >/dev/null

T0=$(date +%s)
# Each class writes its raw profiles into its own directory (<raw dir>/<class>, the engine
# restarted into it: a restart over a store with data in it, recovery and the catalog's reload,
# is trained at every class's start), so that dist-pgo.sh can weight the classes against each
# other without training again.
# (WPEAK: the work directory's largest size at a class's end, in MB.)
WPEAK=0
wsize() { k=$(du -sm "$WORK" | cut -f1); [ "$k" -le "$WPEAK" ] || WPEAK=$k; }
class() { wsize; stop; PROFDIR=$RAW/$1; mkdir -p "$PROFDIR"; start; }
# Startup: a new data directory (bootstrap), first sessions, a restart that recovers it.
PROFDIR=$RAW/startup; mkdir -p "$PROFDIR"
start
b createdb
if want startup; then
  b startup; b startup-catalog
  stop; start; b startup-catalog
fi
b oltp-load --rows 100000

class point
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
class range
# Range reads: 100-row index ranges, a secondary index's aggregate.
if want range; then
  b oltp --rows 100000 --clients 8 --secs 25 --only range_scan_100
  b oltp --rows 100000 --clients 8 --secs 5 --only secondary_lookup
fi
class bulk
# Bulk: COPY in and out, index builds, INSERT ... SELECT, UPDATE, DELETE, VACUUM, ANALYZE.
if want bulk; then
  psql -d bench -c "ALTER DATABASE bench SET maintenance_work_mem = '256MB'" >/dev/null
  b bulk --rows 1000000 --reps 1
  # COPY into indexed tables, the bulk script's copy_pk and copy_sx (BENCH "Bulk writes"): 1M
  # rows into a table with a primary key and a secondary index (rows wait for the unique
  # check), then into one with the secondary index only (held rows, the sink).
  awk 'BEGIN { for (i = 1; i <= 1000000; i++) printf "%d\t%d\tsome text value %d\t%.17g\n", i, (i * 7919) % 100000, i % 1000, (i % 1000) / 7 }' >"$WORK/w/bulk1m.tsv"
  psql -d bench -c "CREATE TABLE bulkp (id int PRIMARY KEY, a int NOT NULL, b text, c float8); CREATE INDEX bulkp_a ON bulkp (a)"
  psql -d bench -c "\\copy bulkp FROM '/w/bulk1m.tsv'"
  psql -d bench -c "CREATE TABLE bulks (id int NOT NULL, a int NOT NULL, b text, c float8); CREATE INDEX bulks_a ON bulks (a)"
  psql -d bench -c "\\copy bulks FROM '/w/bulk1m.tsv'"
  psql -d bench -c "DROP TABLE bulkp, bulks"
  rm -f "$WORK/w/bulk1m.tsv"
  # TPC-B-like's tables at the benchmarked scale, for the short class (pgbench's load: COPY of
  # 3.2M accounts, VACUUM, the primary keys).
  psql -d postgres -c "CREATE DATABASE tpcb"
  pgbench -i -I dtGvp -s 32 tpcb
fi
class short
# Short write transactions: the bench client's UPDATE / SELECT / INSERT transaction at 8, 32
# and 1 clients, pgbench's tpcb-like, single-row INSERT, UPDATE, upsert and DELETE; then
# tpcb-like at the benchmarked scale (32: 3.2M accounts, past the size model's 400,000 versions,
# BENCH "Short write transactions") and its single-row UPDATE alone.
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
  [ -n "$(psql -d tpcb -Atc "SELECT 1 FROM pg_class WHERE relname = 'pgbench_accounts'" 2>/dev/null)" ] \
    || { psql -d postgres -c "CREATE DATABASE tpcb" >/dev/null; pgbench -i -I dtGvp -s 32 tpcb; }
  pgbench -n -M prepared -c 8 -j 4 -T 15 tpcb
  pgbench -n -M prepared -f /w/update.sql -D scale=32 -c 8 -j 4 -T 10 tpcb
fi
psql -d postgres -c "DROP DATABASE IF EXISTS tpcb" >/dev/null
class tpch
# TPC-H at the benchmarked scale (PGO_TPCH_SF, default 1, as BENCH's tables): the load (COPY,
# indexes, VACUUM ANALYZE), all 22 queries, parallel. At SF 0.2 the plans' and batches' shapes
# that SF 1 takes went untrained (PGO took more instructions than plain dist on q01: HANDOFF
# "Analytic CPU, round 6").
if want tpch; then
  b tpch-load --sf "${PGO_TPCH_SF:-1}"
  b tpch --reps 2
fi
class catalog
# Catalog: DDL as a schema grows, psql's \d family and ORM introspection (catalog-tools),
# psql's own queries.
if want catalog; then
  b scale --tables 1500 --schemas 20 --parts 100 --step 500 --reps 2 --keep 1
  b catalog-tools --tables 1500 --schemas 20 --reps 3
  for c in '\d' '\dt+' '\di' '\df' '\dn' '\l' '\du' '\d+ accounts' '\d events' '\d+ lineitem' '\dv' '\dT'; do
    psql -d bench -c "$c" >>"$WORK/out/meta.out" 2>&1 || log "psql $c failed"
  done
fi
class regress
# A slice of the golden SQL suite for breadth: the files whose name's CRC (cksum) is 0 mod 22,
# 34 of 741 today (a set that adding a file doesn't reshuffle, as "every 25th" did), each in a new
# database.
if want regress; then
  n=0
  for f in "$SQL"/*.sql; do
    base=$(basename "$f" .sql)
    [ $(( $(printf '%s' "$base" | cksum | cut -d' ' -f1) % 22 )) = 0 ] || continue
    n=$((n + 1))
    psql -d postgres -c "CREATE DATABASE t_$n" >/dev/null
    psql -d "t_$n" -f "/sql/$base.sql" >"$WORK/out/regress-$base.out" 2>&1 || true
    psql -d postgres -c "DROP DATABASE t_$n" >/dev/null 2>&1 || true
    log "regress $base"
  done
fi
wsize
stop
# Breadth: general SQL, into its own raw profiles.
if [ -n "$BRAW" ] && want breadth; then
  PROFDIR=$BRAW; start
  # Every golden file, four at a time, each in a new database.
  ls "$SQL" | sed -n 's/\.sql$//p' | xargs -P 4 -I{} sh -c '
    d=g_$(echo "{}" | tr -c "a-zA-Z0-9_\n" _)
    x() { timeout 300 docker exec -e PGPASSWORD=pw '"$CLI"' psql -X -q -h 127.0.0.1 -p '"$PORT"' -U postgres "$@"; }
    x -d postgres -c "CREATE DATABASE $d" >/dev/null 2>&1
    x -d "$d" -f "/sql/{}.sql" >/dev/null 2>&1
    x -d postgres -c "DROP DATABASE $d" >/dev/null 2>&1; true'
  log "golden: $(ls "$SQL"/*.sql | wc -l) files $(date +%T)"
  # PostgreSQL's regression suite in its schedule's order, a group four at a time.
  RS=${PGO_REGRESS_SRC:-$(dirname "$RAW")/pgsrc/src/test/regress}
  if [ ! -f "$RS/parallel_schedule" ] && [ -z "${PGO_REGRESS_SRC:-}" ]; then
    rm -rf "$(dirname "$RAW")/pgsrc"
    git clone -q --depth 1 --branch REL_18_6 --filter=blob:none --sparse \
      https://github.com/postgres/postgres.git "$(dirname "$RAW")/pgsrc"
    git -C "$(dirname "$RAW")/pgsrc" sparse-checkout set src/test/regress
  fi
  cp -a "$RS" "$WORK/regress"; mkdir -p "$WORK/regress/results"
  psql -d postgres -c "CREATE DATABASE regression" >/dev/null
  rx() { xargs -P 4 -I{} sh -c 'timeout 300 docker exec -e PGPASSWORD=pw -e PG_ABS_SRCDIR='"$WORK/regress"' \
    -e PG_ABS_BUILDDIR='"$WORK/regress"' -e PG_LIBDIR='"$WORK/regress"' -e PG_DLSUFFIX=.so '"$CLI"' \
    psql -X -q -h 127.0.0.1 -p '"$PORT"' -U postgres -d regression -f '"$WORK/regress"'/sql/{}.sql >/dev/null 2>&1; true'; }
  sed -n 's/^test: *//p' "$WORK/regress/parallel_schedule" | while read -r group; do
    printf '%s\n' $group | rx
  done
  log "regress: $(sed -n 's/^test: *//p' "$WORK/regress/parallel_schedule" | wc -w) files $(date +%T)"
  stop
  echo "pgo-train: breadth $(ls "$BRAW"/*.profraw | wc -l) raw profiles"
fi
T1=$(date +%s)
n=$(ls "$RAW"/*/*.profraw 2>/dev/null | wc -l)
[ "$n" -gt 0 ] || { echo "pgo-train: no profile written (an uninstrumented engine?)" >&2; exit 1; }
echo "pgo-train: $((T1 - T0)) s, $n raw profiles, $(du -sh "$RAW" | cut -f1), work dir up to $WPEAK MB"
rm -rf "$WORK"
