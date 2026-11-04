#!/bin/bash
# conn1000.sh <label> [n]: the engine (bin/<label>) in a container with a 1,024 open-file limit
# (soft and hard), n (1,000) sessions held open by pgbench; prints the engine's open descriptors,
# its cgroup memory and pgbench's outcome.
set -euo pipefail
l=$1; n=${2:-1000}
D=${CONN_DATA:-$HOME/.cache/a165/conn-$l}; bin=$HOME/.cache/a165/bin/$l; port=53306
IMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
EIMG=debian@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
if ss -ltn | grep -q ":$port "; then echo "port $port busy"; exit 1; fi
if docker inspect a165-conn >/dev/null 2>&1; then echo "existing a165-conn; refusing to replace" >&2; exit 1; fi
test ! -e "$D/data"
mkdir -p "$D/data"
created_id=
trap 'if [[ -n "$created_id" ]]; then docker logs "$created_id" >"$D/engine.log" 2>&1 || true; docker rm -f "$created_id" >/dev/null; fi' EXIT
created_id=$(docker run -d --memory 4g --label agent.worktree=agent-a640c14a8f76f8ea4 --name a165-conn --cpuset-cpus 8-11 --cpu-shares 2 --network host --ulimit nofile=1024:1024 \
  --user "$(id -u):$(id -g)" -v "$bin:$bin:ro" -v "$D:$D" -v /usr/share/zoneinfo:/usr/share/zoneinfo:ro -e TMPDIR="$D/data" \
  "$EIMG" "$bin/roll-engine" --data "$D/data" --listen 127.0.0.1:$port --password pw -c max_connections=1100)
i=0; until ss -ltn | grep -q ":$port "; do sleep 0.5; i=$((i+1)); [ $i -lt 120 ] || { echo "no start"; exit 1; }; done
pid=$(docker inspect -f '{{.State.Pid}}' a165-conn)
cg=/sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' a165-conn).scope
printf 'SELECT 1;\n\\sleep 40 s\n' >"$D/idle.sql"
idle_fds=$(docker exec a165-conn sh -c 'ls /proc/1/fd | wc -l')
echo "idle fds: $idle_fds"
docker run --rm --network host --ulimit nofile=8192:8192 --cpuset-cpus 12-13,18-19 -e PGPASSWORD=pw -v "$D:$D" "$IMG" \
  pgbench -h 127.0.0.1 -p $port -U postgres -n -c "$n" -j 8 -t 1 -f "$D/idle.sql" postgres >"$D/pgbench.log" 2>&1 &
bp=$!
sleep 30
connected_fds=$(docker exec a165-conn sh -c 'ls /proc/1/fd | wc -l')
test "$connected_fds" -eq "$((idle_fds+n))"
echo "fds with $n sessions: $connected_fds  anon+shmem MB: $(awk '$1 == "anon" || $1 == "shmem" {s += $2} END {printf "%.1f", s / 1048576}' "$cg/memory.stat")"
wait "$bp"
grep -q "number of transactions actually processed: $n/$n" "$D/pgbench.log"
grep -q "number of failed transactions: 0 " "$D/pgbench.log"
grep -E 'processed|failed|error|tps' "$D/pgbench.log" | head -5
docker logs a165-conn >"$D/engine.log" 2>&1
if grep -qi 'too many open files' "$D/engine.log"; then cat "$D/engine.log"; exit 1; fi
docker rm -f "$created_id" >/dev/null
created_id=
# Preserve measured client and server logs for audit.
