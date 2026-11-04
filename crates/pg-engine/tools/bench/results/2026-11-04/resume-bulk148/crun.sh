#!/bin/sh
# crun.sh <rounds> <out> : the bulk steps on the dist engine in a container and on PostgreSQL in a
# container, same cpuset and CPU weight (tools/bench/run.sh ENGINE_DOCKER=1), interleaved.
A=$HOME/.cache/a148
B=/home/meszmate/.cache/pg-engine-resume-20261104/execution_models/bulk148
R=/home/meszmate/roll/.claude/worktrees/agent-ad1ff70de0f5338c1/crates/pg-engine/tools/bench/run.sh
rounds=$1; out=$2
export DATA=$A/rb BENCH_DATA=$A/rb SERVER_CPUS=8-11 CLIENT_CPUS=12-15 PG_PORT=52966 ENGINE_PORT=52967 \
  PG_NAME=a148-bpg ENGINE_NAME=a148-beng PG_CPU_SHARES=2 PG_NETWORK=host BENCH_BIN=$A/t-dist/dist
export PG_PORT_T=52966 PG_NAME_T=a148-bpg
: >"$out"
for p in 52966 52967; do if ss -ltn | grep -q ":$p "; then echo "port $p busy" >>"$out"; exit 1; fi; done
sh "$R" pg-start >>"$out" 2>&1
docker inspect a148-bpg > "/home/meszmate/.cache/pg-engine-resume-20261104/execution_models/bulk148/pg-container.json"
r=1
while [ $r -le "$rounds" ]; do
  rm -rf "$A/rb/engine"
  ENGINE_DOCKER=1 sh "$R" engine-start >/dev/null 2>&1
  mkdir -p "$A/eng-cont"; cp "$A/rb/engine.pid" "$A/eng-cont/pid"; echo 52967 >"$A/eng-cont/port"; ln -sfn "$A/rb/engine" "$A/eng-cont/data"
  bash "$B/bulk.sh" cont "eng-$r" >>"$out" 2>&1
  docker inspect a148-beng > "/home/meszmate/.cache/pg-engine-resume-20261104/execution_models/bulk148/engine-container-$r.json"
  id=$(docker inspect -f '{{.Id}}' a148-beng)
  echo "eng-$r	cgroup memory.peak MB $(( $(cat /sys/fs/cgroup/system.slice/docker-$id.scope/memory.peak) / 1048576 ))" >>"$out"
  ENGINE_DOCKER=1 sh "$R" engine-stop >/dev/null 2>&1
  bash "$B/bulk.sh" pg "pg-$r" >>"$out" 2>&1
  r=$((r + 1))
done
sh "$R" pg-stop >/dev/null 2>&1
rm -rf "$A/rb/engine"
echo done >>"$out"
