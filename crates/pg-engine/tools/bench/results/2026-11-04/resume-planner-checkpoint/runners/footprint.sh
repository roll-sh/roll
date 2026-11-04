#!/bin/bash
# footprint.sh <label> <bin-dir> [runs]: scoreboard.sh's footprint gate for the engine alone
# (fresh data dir MB, idle CPU over 60 s scaled to 5 min, anon+shmem MB with 100 idle sessions),
# the engine in a container as run.sh's ENGINE_DOCKER runs it. Prints GATE lines.
set -euo pipefail
label=$1; bin=$2; runs=${3:-1}
E=/home/meszmate/roll/.claude/worktrees/agent-a640c14a8f76f8ea4/crates/pg-engine
export BENCH_DATA=$HOME/.cache/a165/fp-$label BENCH_BIN=$bin ENGINE_DOCKER=1
export ENGINE_NAME=a165-fp-engine ENGINE_PORT=${ENGINE_PORT:-53301} SERVER_CPUS=${SERVER_CPUS:-8-11}
export CLIENT_CPUS=${CLIENT_CPUS:-12-13} PG_CPU_SHARES=2 PG_NAME=a165-fp-pg
IMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
RUN=$E/tools/bench/run.sh
mkdir -p "$BENCH_DATA/logs"
printf 'SELECT 1;\n\\sleep 40 s\n' >"$BENCH_DATA/idle-session.sql"
calc() { awk "BEGIN {printf \"%.3f\", $1}"; }
for r in $(seq "$runs"); do
  "$RUN" engine-stop; rm -rf "$BENCH_DATA/engine"
  "$RUN" engine-start >>"$BENCH_DATA/logs/fp.log" 2>&1 || { echo "start failed"; exit 1; }
  cg=/sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' "$ENGINE_NAME").scope
  mem() { awk '$1 == "anon" || $1 == "shmem" {s += $2} END {print s + 0}' "$cg/memory.stat"; }
  cpu() { awk '/^usage_usec/ {printf "%.3f", $2 / 1e6}' "$cg/cpu.stat"; }
  kb=$(du -sk "$BENCH_DATA/engine" | awk '{print $1}')
  sleep 10
  c0=$(cpu); sleep 60; c1=$(cpu)
  m0=$(mem)
  docker run --rm --network host --cpuset-cpus "$CLIENT_CPUS" -e PGPASSWORD=pw -v "$BENCH_DATA:$BENCH_DATA" "$IMG" \
    pgbench -h 127.0.0.1 -p "$ENGINE_PORT" -U postgres -n -c 100 -j 4 -t 1 -f "$BENCH_DATA/idle-session.sql" postgres >>"$BENCH_DATA/logs/fp.log" 2>&1 &
  bp=$!
  sleep 25
  m1=$(mem)
  printf 'GATE\t%s\trun=%s\tfresh_data_mb=%s\tidle_cpu_s_5min=%s\tno_sessions_mb=%s\tidle_100_sessions_mb=%s\n' "$label" "$r" \
    "$(calc "$kb / 1024")" "$(calc "($c1 - $c0) * 5")" "$(calc "$m0 / 1048576")" "$(calc "$m1 / 1048576")"
  wait "$bp"
  "$RUN" engine-stop
done
rm -rf "$BENCH_DATA/engine"
