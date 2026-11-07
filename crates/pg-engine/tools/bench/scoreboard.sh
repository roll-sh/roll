#!/bin/bash
# The scoreboard: every tools/bench workload against PostgreSQL 18.6 on the same pinned cores,
# in interleaved rounds, and the footprint gate. See BENCH.md ("Scoreboard").
#
#   tools/bench/scoreboard.sh quick [--update]   # agents: small sizes, 2 rounds
#   tools/bench/scoreboard.sh full  [--update]   # releases: suite.sh's sizes, 3 rounds (hours)
#
# Per workload phase it measures, around the phase on whichever server runs it: the server's
# CPU (the engine process's; PostgreSQL's container cgroup), the bytes it wrote (/proc/<pid>/io
# write_bytes; the cgroup's io.stat wbytes; a CHECKPOINT ends every phase on both), its peak
# memory (RSS; the cgroup's anon + shmem, sampled every 0.5 s) and the work done (tps and
# operations of fixed-time phases, the wall time of fixed-work ones). Rounds alternate which
# server goes first. scoreboard.py turns the SCORE / GATE lines into the table; --update
# writes it into BENCH.md between the scoreboard markers.
#
# The footprint gate runs first, on fresh servers, one at a time: idle CPU (IDLE_SECS, 300 in
# full mode; quick measures 60 s and scales to 5 minutes), the server's memory with 100 idle
# sessions (pgbench -c 100 of `SELECT 1; \sleep 40 s`), the size of a fresh data directory.
#
# Needs the engine built with the dist profile (fat LTO: what users run; a release engine
# understates it) and the bench client in release, through tools/heavy.sh:
#   tools/heavy.sh cargo build --profile dist -p pg-engine --bin roll-engine
#   tools/heavy.sh cargo build --release -p pg-engine --example bench
# (BENCH_BIN, default <target>/dist, has roll-engine; BENCH_CLIENT, default
# <target>/release/examples/bench, is the client; <target> is CARGO_TARGET_DIR or <repo>/target.)
# Knobs (environment): SB_DATA (results and data, default ~/.cache/scoreboard), SERVER_CPUS /
# CLIENT_CPUS (default 20-23 / 8-11: pick cores no other benchmark pins), PG_PORT /
# ENGINE_PORT (default 55370 / 55371), PG_NAME (sb-pg), ROUNDS, IDLE_SECS, SB_ONLY (a comma
# list of workloads: pgbench,oltp,analytic,tpch,bulk,ddl,connect), SB_COMMIT (the engine's commit
# in the table, default the checkout's HEAD). Everything it starts runs at
# nice 19 (PostgreSQL with --cpu-shares 2, about the same weight) and on the host network.
set -uo pipefail
if [ -z "${BENCH_SG:-}" ] && ! docker info >/dev/null 2>&1; then
  q=""; for a in "$0" "$@"; do q="$q '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"; done
  BENCH_SG=1 exec sg docker -c "BENCH_SG=1 bash $q"
fi
[ -n "${SB_NICED:-}" ] || SB_NICED=1 exec nice -n 19 bash "$0" "$@"
MODE=${1:-quick}; UPDATE=${2:-}
case $MODE in quick|full) ;; *) echo "usage: scoreboard.sh quick|full [--update]" >&2; exit 2 ;; esac
HERE=$(cd "$(dirname "$0")" && pwd)
TARGET=${CARGO_TARGET_DIR:-$(cd "$HERE/../../../.." && pwd)/target}
export BENCH_BIN=${BENCH_BIN:-$TARGET/dist} BENCH_CLIENT=${BENCH_CLIENT:-$TARGET/release/examples/bench}
for b in "$BENCH_BIN/roll-engine" "$BENCH_CLIENT"; do
  [ -x "$b" ] || { echo "missing $b: build it (see the header)" >&2; exit 1; }
done
export BENCH_DATA=${SB_DATA:-$HOME/.cache/scoreboard}
export SERVER_CPUS=${SERVER_CPUS:-20-23} CLIENT_CPUS=${CLIENT_CPUS:-8-11}
export PG_PORT=${PG_PORT:-55370} ENGINE_PORT=${ENGINE_PORT:-55371} PG_NAME=${PG_NAME:-sb-pg}
export PG_NETWORK=host PG_CPU_SHARES=2
# The engine runs in a container of its own, as PostgreSQL does (run.sh's ENGINE_DOCKER): the
# same slice, cpuset and CPU weight. ENGINE_DOCKER= (empty) runs it on the host as before.
export ENGINE_DOCKER=${ENGINE_DOCKER-1} ENGINE_NAME=${ENGINE_NAME:-$PG_NAME-engine}
RUN=$HERE/run.sh
OUT=$BENCH_DATA/score-$MODE.tsv
LOG=$BENCH_DATA/logs/scoreboard-$MODE.log
mkdir -p "$BENCH_DATA/logs"
: >"$OUT"; : >"$LOG"
IMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
if [ "$MODE" = quick ]; then
  ROUNDS=${ROUNDS:-2}; IDLE_SECS=${IDLE_SECS:-60}; SECS=10
else
  ROUNDS=${ROUNDS:-3}; IDLE_SECS=${IDLE_SECS:-300}; SECS=15
fi
ONLY=${SB_ONLY:-pgbench,oltp,analytic,tpch,bulk,ddl,connect}
has() { case ",$ONLY," in *",$1,"*) return 0 ;; esac; return 1; }
for p in "$PG_PORT" "$ENGINE_PORT"; do
  if ss -ltn | grep -q ":$p "; then echo "port $p busy: pick PG_PORT / ENGINE_PORT" >&2; exit 1; fi
done

# ---- probes: CPU seconds, bytes written, memory bytes of the server `t` (pg | engine)
PG_CG=; EN_CG=
cg() { echo "/sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' "$1").scope"; }
epid() { cat "$BENCH_DATA/engine.pid"; }
# (A container's probes are its cgroup's, whichever server it holds: CPU of every process in
# it, bytes written, anon + shmem memory.)
cgof() { if [ "$1" = pg ]; then echo "$PG_CG"; else echo "$EN_CG"; fi; }
cpu() {
  if [ "$1" = pg ] || [ -n "$ENGINE_DOCKER" ]; then awk '/^usage_usec/ {printf "%.3f", $2 / 1e6}' "$(cgof "$1")/cpu.stat"
  else awk -v hz="$(getconf CLK_TCK)" '{sub(/.*\) /, ""); printf "%.3f", ($12 + $13) / hz}' "/proc/$(epid)/stat"; fi
}
wbytes() {
  if [ "$1" = pg ] || [ -n "$ENGINE_DOCKER" ]; then
    awk '{for (i = 2; i <= NF; i++) if ($i ~ /^wbytes=/) {sub(/wbytes=/, "", $i); s += $i}} END {print s + 0}' "$(cgof "$1")/io.stat"
  else awk '/^write_bytes/ {print $2}' "/proc/$(epid)/io"; fi
}
mem() {
  if [ "$1" = pg ] || [ -n "$ENGINE_DOCKER" ]; then awk '$1 == "anon" || $1 == "shmem" {s += $2} END {print s + 0}' "$(cgof "$1")/memory.stat"
  else awk '/^VmRSS/ {print $2 * 1024}' "/proc/$(epid)/status"; fi
}
sampler() { # sampler <t> <file>: the peak of mem() until <file>.stop exists
  local m max=0
  while [ ! -e "$2.stop" ]; do
    m=$(mem "$1"); [ "${m:-0}" -gt "$max" ] && max=$m; echo "$max" >"$2"; sleep 0.5
  done
}
checkpoint() { "$RUN" psql "$1" "CHECKPOINT" >/dev/null 2>&1 || true; }
calc() { awk "BEGIN {printf \"%.3f\", $1}"; }

# ---- one phase on one server: a SCORE line
phase() {
  local r=$1 t=$2 w=$3 kind=$4; shift 4
  local f="$BENCH_DATA/peak.$$" c0 c1 w0 w1 s0 s1 out tps ops ok=1 sp
  rm -f "$f" "$f.stop"; echo 0 >"$f"
  sampler "$t" "$f" & sp=$!
  c0=$(cpu "$t"); w0=$(wbytes "$t"); s0=$(date +%s.%N)
  out=$("$@" 2>&1) || ok=0
  checkpoint "$t"
  s1=$(date +%s.%N); c1=$(cpu "$t"); w1=$(wbytes "$t")
  touch "$f.stop"; wait "$sp"
  printf '%s\n' "$out" | sed "s/^/[$r $t $w] /" >>"$LOG"
  if echo "$out" | grep -qE $'\terror=|\terrors=[1-9]|^ERROR|FATAL|failed transactions: [1-9]'; then ok=0; fi
  # (Fixed-time phases: pgbench's tps and transactions; the bench client's tps and ops.)
  tps=$(echo "$out" | sed -n -e 's/^tps = \([0-9.]*\).*/\1/p' -e 's/.*\ttps=\([0-9.]*\).*/\1/p' | head -1)
  ops=$(echo "$out" | sed -n -e 's/^number of transactions actually processed: \([0-9]*\).*/\1/p' -e 's/.*\tops=\([0-9]*\).*/\1/p' | head -1)
  printf 'SCORE\t%s\t%s\t%s\tkind=%s\tsecs=%s\ttps=%s\tops=%s\tcpu_s=%s\twrite_mb=%s\tpeak_mb=%s\tok=%s\n' \
    "$r" "$t" "$w" "$kind" "$(calc "$s1 - $s0")" "${tps:-}" "${ops:-}" "$(calc "$c1 - $c0")" \
    "$(calc "($w1 - $w0) / 1048576")" "$(calc "$(cat "$f") / 1048576")" "$ok" | tee -a "$OUT"
  rm -f "$f" "$f.stop"
}

start() { # a fresh server
  if [ "$1" = pg ]; then "$RUN" pg-stop; "$RUN" pg-start >>"$LOG" 2>&1; PG_CG=$(cg "$PG_NAME")
  else
    "$RUN" engine-stop; rm -rf "$BENCH_DATA/engine"; "$RUN" engine-start >>"$LOG" 2>&1
    if [ -n "$ENGINE_DOCKER" ]; then EN_CG=$(cg "$ENGINE_NAME"); fi
  fi
}
stop() { if [ "$1" = pg ]; then "$RUN" pg-stop; else "$RUN" engine-stop; fi; }
trap 'stop pg; stop engine' EXIT

# ---- the footprint gate, one fresh server at a time
printf 'SELECT 1;\n\\sleep 40 s\n' >"$BENCH_DATA/idle-session.sql"
for t in pg engine; do
  start "$t"
  if [ "$t" = pg ]; then
    kb=$(docker exec "$PG_NAME" sh -c 'du -sk "$PGDATA"' | awk '{print $1}')
  else
    kb=$(du -sk "$BENCH_DATA/engine" | awk '{print $1}')
  fi
  printf 'GATE\t%s\tfresh_data_mb\t%s\n' "$t" "$(calc "$kb / 1024")" | tee -a "$OUT"
  sleep 10
  c0=$(cpu "$t"); sleep "$IDLE_SECS"; c1=$(cpu "$t")
  printf 'GATE\t%s\tidle_cpu_s_5min\t%s\n' "$t" "$(calc "($c1 - $c0) * 300 / $IDLE_SECS")" | tee -a "$OUT"
  port=$PG_PORT; [ "$t" = engine ] && port=$ENGINE_PORT
  docker run --rm --network host --cpuset-cpus "$CLIENT_CPUS" -e PGPASSWORD=pw -v "$BENCH_DATA:$BENCH_DATA" "$IMG" \
    pgbench -h 127.0.0.1 -p "$port" -U postgres -n -c 100 -j 4 -t 1 -f "$BENCH_DATA/idle-session.sql" postgres >>"$LOG" 2>&1 &
  bp=$!
  sleep 25
  printf 'GATE\t%s\tidle_100_sessions_mb\t%s\n' "$t" "$(calc "$(mem "$t") / 1048576")" | tee -a "$OUT"
  wait "$bp"
  stop "$t"
done

# ---- the workloads, both servers up, rounds interleaved
start pg; start engine
for t in pg engine; do "$RUN" bench "$t" --label sb createdb --db bench >>"$LOG" 2>&1; done
if has tpch; then for t in pg engine; do "$RUN" bench "$t" --label sb createdb --db tpch >>"$LOG" 2>&1; done; fi
if [ "$MODE" = quick ]; then
  PGB_SCALES="1"; PGB_CLIENTS="1 8 32"; OLTP_ROWS=100000; OLTP_SECS=5; OLTP_CLIENTS="8"
  OLTP_CASES="point_select range_scan_100 insert update_indexed"; OLTP_C32="update_indexed"; AN_ROWS="200000"; AN_REPS=1
  SF=0.05; TPCH_REPS=1; BULK_ROWS="100000"; BULK_REPS=1; DDL="--tables 100 --reps 3"
else
  PGB_SCALES="1 10"; PGB_CLIENTS="1 8 32"; OLTP_ROWS=1000000; OLTP_SECS=8; OLTP_CLIENTS="1 8"
  OLTP_CASES="point_select point_select_simple range_scan_100 secondary_lookup select_1 insert update_indexed upsert short_txn"
  OLTP_C32="insert update_indexed short_txn"
  AN_ROWS="200000 2000000"; AN_REPS=3; SF=1; TPCH_REPS=3; BULK_ROWS="100000 1000000"; BULK_REPS=2
  DDL="--tables 500 --reps 20"
fi
both() { # both <round> <workload> <kind> <command, @T@ standing for the server>...
  local r=$1 w=$2 kind=$3 t; shift 3
  local order="pg engine"; [ $((r % 2)) -eq 0 ] && order="engine pg"
  for t in $order; do phase "$r" "$t" "$w" "$kind" "${@//@T@/$t}"; done
}
for r in $(seq 1 "$ROUNDS"); do
  if has pgbench; then
    for s in $PGB_SCALES; do
      both "$r" "pgbench init s$s" work "$RUN" pgbench @T@ -i -I dtGvp -s "$s" bench
      for mode in select tpcb; do
        flag=""; [ "$mode" = select ] && flag=-S
        for c in $PGB_CLIENTS; do
          j=$c; [ "$j" -gt 4 ] && j=4
          # shellcheck disable=SC2086
          both "$r" "pgbench s$s $mode c$c" time "$RUN" pgbench @T@ -n -M prepared $flag -c "$c" -j "$j" -T "$SECS" bench
        done
      done
    done
  fi
  if has oltp; then
    both "$r" "oltp load" work "$RUN" bench @T@ --label sb oltp-load --rows "$OLTP_ROWS"
    for case in $OLTP_CASES; do
      for c in $OLTP_CLIENTS; do
        both "$r" "oltp $case c$c" time "$RUN" bench @T@ --label sb oltp --rows "$OLTP_ROWS" --secs "$OLTP_SECS" --clients "$c" --only "$case"
      done
    done
    # Write-heavy at 32 clients (pgbench's TPC-B-like at 32 above contends on scale 1's single branch row).
    for case in $OLTP_C32; do
      both "$r" "oltp $case c32" time "$RUN" bench @T@ --label sb oltp --rows "$OLTP_ROWS" --secs "$OLTP_SECS" --clients 32 --only "$case"
    done
  fi
  if has analytic; then
    for rows in $AN_ROWS; do
      both "$r" "analytic load $rows" work "$RUN" bench @T@ --label sb analytic-load --rows "$rows"
      both "$r" "analytic $rows" work "$RUN" bench @T@ --label sb analytic --rows "$rows" --reps "$AN_REPS" --timeout 600
    done
  fi
  if has tpch; then
    if [ "$r" -eq 1 ]; then both "$r" "tpch load sf$SF" work "$RUN" bench @T@ --label sb tpch-load --sf "$SF" --db tpch; fi
    both "$r" "tpch sf$SF" work "$RUN" bench @T@ --label sb tpch --reps "$TPCH_REPS" --timeout 900 --db tpch
  fi
  if has bulk; then
    for rows in $BULK_ROWS; do
      both "$r" "bulk $rows" work "$RUN" bench @T@ --label sb bulk --rows "$rows" --reps "$BULK_REPS"
    done
  fi
  if has ddl; then
    # shellcheck disable=SC2086
    both "$r" "ddl" work "$RUN" bench @T@ --label sb ddl $DDL
  fi
  if has connect; then
    both "$r" "connect 100 sessions" work "$RUN" bench @T@ --label sb idle --sessions 100 --dbs 10
  fi
done
stop pg; stop engine; trap - EXIT
python3 "$HERE/scoreboard.py" "$OUT" "$MODE" "$ROUNDS" "$IDLE_SECS" ${UPDATE:+"$UPDATE"}
