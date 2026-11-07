#!/bin/sh
# The fdw traffic workload on PostgreSQL 18.6 and on the engine, both servers in containers
# (the same cpuset, CPU weight, slice and host network; run.sh's ENGINE_DOCKER way), each
# server's postgres_fdw looping back to itself through the counting proxy: round trips, bytes,
# time, and each server's CPU and peak memory from its cgroup. BENCH.md "postgres_fdw, round 7".
#
#   tools/fdwtraffic/compare.sh <dist roll-engine> <scratch dir> <name prefix> <base port> [pg|engine]...
#
# Ports base..base+3 must be free. SERVER_CPUS / CLIENT_CPUS pick the cores (default 4-7 / 8-9).
# Containers <prefix>-pg, <prefix>-eng, <prefix>-psql are made and removed by this script.
set -u
eng=$1; T=$2; N=$3; B=$4; shift 4
SIDES=${*:-pg engine}
H=$(cd "$(dirname "$0")" && pwd)
L=$(printf '%d.%d.%d.%d' 127 0 0 1)
CPUS=${SERVER_CPUS:-4-7}; CCPUS=${CLIENT_CPUS:-8-9}
PG_IMAGE=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
ENGINE_IMAGE=debian@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
for p in $B $((B + 1)) $((B + 2)) $((B + 3)); do
  ss -ltn | grep -q ":$p " && { echo "port $p busy" >&2; exit 1; }
done
mkdir -p "$T"
docker rm -f "$N-psql" >/dev/null 2>&1
docker run -d --name "$N-psql" --network host --cpuset-cpus "$CCPUS" -v "$T:$T" \
  -v /sys/fs/cgroup:/hostcg:ro --entrypoint sleep "$PG_IMAGE" infinity >/dev/null
run() { # side port container
  side=$1; port=$2; c=$3
  id=$(docker inspect -f '{{.Id}}' "$c")
  nice -n 19 python3 -I "$H/fdwproxy.py" $((port + 1)) "$port" "$T/$side.counters" > "$T/$side.proxy.log" 2>&1 &
  ppid=$!
  i=0; until ss -ltn | grep -q ":$((port + 1)) "; do sleep 0.2; i=$((i + 1)); [ $i -lt 100 ] || break; done
  python3 -I "$H/pgtraffic.py" gen $((port + 1)) "$T/$side.counters" "/hostcg/system.slice/docker-$id.scope" > "$T/$side.sql"
  docker exec -i -e PGPASSWORD=pw "$N-psql" psql -X -h $L -p "$port" -U postgres -d postgres < "$T/$side.sql" > "$T/$side.out" 2>&1
  echo "== $side"
  python3 -I "$H/pgtraffic.py" sum < "$T/$side.out"
  grep -i 'error' "$T/$side.out" | head -3
  kill $ppid
}
for side in $SIDES; do
  if [ "$side" = pg ]; then
    docker rm -f "$N-pg" >/dev/null 2>&1
    docker run -d --name "$N-pg" --cpuset-cpus "$CPUS" --cpu-shares 2 --shm-size=1g --network host \
      -e POSTGRES_PASSWORD=pw -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" "$PG_IMAGE" \
      -c port=$B -c listen_addresses=$L -c shared_buffers=2GB -c maintenance_work_mem=256MB \
      -c max_wal_size=8GB -c checkpoint_timeout=30min -c max_connections=300 >/dev/null
    i=0; until docker exec "$N-pg" pg_isready -U postgres -h $L -p $B -q 2>/dev/null; do sleep 1; i=$((i + 1)); [ $i -lt 60 ] || break; done
    sleep 2
    run pg $B "$N-pg"
    docker rm -f -v "$N-pg" >/dev/null
  else
    rm -rf "$T/engine"; mkdir -p "$T/engine"
    d=$(dirname "$eng")
    docker rm -f "$N-eng" >/dev/null 2>&1
    docker run -d --name "$N-eng" --cpuset-cpus "$CPUS" --cpu-shares 2 --network host \
      --user "$(id -u):$(id -g)" -v "$d:$d:ro" -v "$T/engine:$T/engine" \
      -v /usr/share/zoneinfo:/usr/share/zoneinfo:ro -e TMPDIR="$T/engine" "$ENGINE_IMAGE" \
      "$eng" --data "$T/engine" --listen $L:$((B + 2)) --allow-outbound $L:$((B + 3)) \
      --password pw --cache-mb 2048 >/dev/null
    i=0; until ss -ltn | grep -q ":$((B + 2)) "; do sleep 0.2; i=$((i + 1)); [ $i -lt 300 ] || break; done
    run engine $((B + 2)) "$N-eng"
    docker logs "$N-eng" > "$T/engine.log" 2>&1
    docker stop -t 30 "$N-eng" >/dev/null; docker rm -f "$N-eng" >/dev/null
  fi
done
docker rm -f "$N-psql" >/dev/null
