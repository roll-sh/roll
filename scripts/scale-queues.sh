#!/usr/bin/env bash
# What idle queues and connections cost: roll queues vs RabbitMQ 4, each in a container with the
# same CPU and memory limits, one broker at a time. For each broker it measures memory and idle
# CPU when empty, after declaring QUEUES durable classic queues, while also holding CONNS idle
# connections (one channel each), and once those clients have left (queues still loaded). Memory is the container's anonymous memory (the process
# heaps; page cache is not counted). CPU is CPU-seconds per minute while nothing is happening.
# Writes docs/benchmarks/queues-scale.md from every run in target/scale-queues.
#
#   scripts/scale-queues.sh                         # 20k queues, 5k connections, both brokers
#   QUEUES=200000 CONNS=50000 BROKERS=roll scripts/scale-queues.sh
#   REPORT_ONLY=1 scripts/scale-queues.sh
set -euo pipefail
cd "$(dirname "$0")/.."

QUEUES=${QUEUES:-20000}
CONNS=${CONNS:-5000}
CPUS=${CPUS:-4}
MEM=${MEM:-8g}
SETTLE=${SETTLE:-15}
WINDOW=${WINDOW:-30}
BROKERS=${BROKERS:-roll rabbitmq}
NET=roll-bench
RABBIT_IMAGE=${RABBIT_IMAGE:-rabbitmq:4-management}
PY_IMAGE=${PY_IMAGE:-python:3.12-alpine}
OUT=docs/benchmarks/queues-scale.md
RAW=$PWD/target/scale-queues
mkdir -p "$RAW" docs/benchmarks

cleanup() {
  rm -f "$RAW/hold"
  docker rm -f -v scale-broker scale-conns >/dev/null 2>&1 || true
}
trap cleanup EXIT

driver() {
  docker run --rm --network $NET -v "$PWD/scripts/scale-queues.py:/scale.py:ro" -v "$RAW:/raw" "$PY_IMAGE" \
    python3 /scale.py "$@"
}

# Anonymous memory of the broker container, in MiB.
mem() {
  docker exec scale-broker cat /sys/fs/cgroup/memory.stat | awk '$1 == "anon" { printf "%.1f", $2 / 1048576 }'
}

# CPU-seconds per minute used by the broker over WINDOW seconds of nothing happening.
idle_cpu() {
  local a b
  a=$(docker exec scale-broker cat /sys/fs/cgroup/cpu.stat | awk '$1 == "usage_usec" { print $2 }')
  sleep "$WINDOW"
  b=$(docker exec scale-broker cat /sys/fs/cgroup/cpu.stat | awk '$1 == "usage_usec" { print $2 }')
  awk -v a="$a" -v b="$b" -v w="$WINDOW" 'BEGIN { printf "%.2f", (b - a) / 1e6 * 60 / w }'
}

measure() {
  local broker=$1
  cleanup
  case $broker in
    roll) docker run -d --name scale-broker --network $NET --cpus "$CPUS" --memory "$MEM" \
            -e ROLL_QUEUES_USER=bench -e ROLL_QUEUES_PASSWORD=bench roll-queues:bench >/dev/null ;;
    rabbitmq) docker run -d --name scale-broker --network $NET --cpus "$CPUS" --memory "$MEM" \
            -e RABBITMQ_DEFAULT_USER=bench -e RABBITMQ_DEFAULT_PASS=bench "$RABBIT_IMAGE" >/dev/null ;;
  esac
  local ok=
  for _ in $(seq 1 120); do
    if driver queues scale-broker bench bench 0 >/dev/null 2>&1; then ok=1; break; fi
    sleep 1
  done
  [ -n "$ok" ] || { echo "scale: $broker did not start" >&2; docker logs scale-broker | tail -20 >&2; exit 1; }

  sleep "$SETTLE"
  local m0 c0 m1 c1 m2 c2 c3 t0 t1 declare_s
  m0=$(mem); c0=$(idle_cpu)
  echo "  empty: ${m0} MiB, ${c0} CPU-s/min"

  t0=$(date +%s)
  driver queues scale-broker bench bench "$QUEUES"
  t1=$(date +%s)
  declare_s=$((t1 - t0))
  sleep "$SETTLE"
  m1=$(mem); c1=$(idle_cpu)
  echo "  $QUEUES queues (declared in ${declare_s}s): ${m1} MiB, ${c1} CPU-s/min"

  rm -f "$RAW/hold"
  docker run -d --name scale-conns --network $NET --ulimit nofile=1048576:1048576 \
    -v "$PWD/scripts/scale-queues.py:/scale.py:ro" -v "$RAW:/raw" "$PY_IMAGE" \
    python3 /scale.py connections scale-broker bench bench "$CONNS" /raw/hold >/dev/null
  for _ in $(seq 1 600); do
    grep -q ready "$RAW/hold" 2>/dev/null && break
    if [ "$(docker inspect -f '{{.State.Running}}' scale-conns 2>/dev/null)" != true ]; then
      echo "scale: the connection driver stopped" >&2; docker logs scale-conns | tail -20 >&2; exit 1
    fi
    sleep 1
  done
  grep -q ready "$RAW/hold" || { echo "scale: $CONNS connections did not open" >&2; exit 1; }
  sleep "$SETTLE"
  m2=$(mem); c2=$(idle_cpu)
  echo "  + $CONNS connections: ${m2} MiB, ${c2} CPU-s/min"
  rm -f "$RAW/hold"
  docker wait scale-conns >/dev/null 2>&1 || true
  sleep "$SETTLE"
  c3=$(idle_cpu)
  echo "  clients gone, $QUEUES queues loaded: ${c3} CPU-s/min"

  echo "$m0 $c0 $m1 $c1 $m2 $c2 $declare_s $c3" >"$RAW/$broker-$QUEUES-$CONNS.res"
}

if [ -z "${REPORT_ONLY:-}" ]; then
  echo "scale: building roll-queues:bench"
  docker build -q -f deploy/queues/Dockerfile -t roll-queues:bench . >/dev/null
  docker network inspect $NET >/dev/null 2>&1 || docker network create $NET >/dev/null
  for b in $BROKERS; do
    echo "scale: $b, $QUEUES queues, $CONNS connections"
    measure "$b"
  done
  cleanup
fi

name() { case $1 in roll) echo "roll queues" ;; rabbitmq) echo "RabbitMQ 4" ;; esac; }
{
  echo "# What idle queues and connections cost: roll queues vs RabbitMQ 4"
  echo
  echo "Run by \`scripts/scale-queues.sh\` on $(uname -s) $(uname -m), $(date +%F). Each broker runs alone in"
  echo "a container limited to $CPUS CPUs and $MEM. Queues are durable classic queues on one vhost, declared"
  echo "over one connection; connections are idle AMQP 0-9-1 connections with one open channel each and"
  echo "heartbeats off. Memory is the container's anonymous memory (process heaps, not page cache). Idle"
  echo "CPU is CPU-seconds per minute over ${WINDOW}s with no traffic."
  echo
  echo "| broker | queues | connections | empty | with queues | with queues + connections | per queue | per connection | declare rate |"
  echo "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
  for f in "$RAW"/*.res; do
    [ -e "$f" ] || continue
    base=$(basename "$f" .res)
    broker=${base%%-*}; rest=${base#*-}; q=${rest%%-*}; c=${rest#*-}
    read -r m0 c0 m1 c1 m2 c2 ds c3 <"$f"
    awk -v b="$(name "$broker")" -v q="$q" -v c="$c" -v m0="$m0" -v m1="$m1" -v m2="$m2" -v ds="$ds" 'BEGIN {
      rate = ds > 0 ? sprintf("%.0f/s", q / ds) : sprintf("> %d/s", q)
      printf "| %s | %d | %d | %.0f MiB | %.0f MiB | %.0f MiB | %.1f KiB | %.1f KiB | %s |\n",
        b, q, c, m0, m1, m2, (m1 - m0) * 1024 / q, (m2 - m1) * 1024 / c, rate
    }'
  done
  echo
  echo "Idle CPU, in CPU-seconds per minute with no traffic. The method has a floor of about 0.02: a"
  echo "container that only sleeps measures that much, because reading its counters runs \`docker exec\`"
  echo "inside it. A roll broker with no clients parks all background work (see"
  echo "[architecture.md](../queues/architecture.md#idle-cost)); with clients connected its samplers and"
  echo "alarm checks run."
  echo
  echo "| broker | empty | with queues | with queues + idle connections | clients gone, queues loaded |"
  echo "| --- | ---: | ---: | ---: | ---: |"
  for f in "$RAW"/*.res; do
    [ -e "$f" ] || continue
    base=$(basename "$f" .res)
    read -r _ c0 _ c1 _ c2 _ c3 <"$f"
    echo "| $(name "${base%%-*}") | $c0 | $c1 | $c2 | ${c3:-–} |"
  done
  echo
  echo "Raw numbers are in \`target/scale-queues/\`. A sleeping vhost (scale to zero) holds no memory"
  echo "for its queues at all; they are loaded again on the next connection."
} >"$OUT"
echo "scale: wrote $OUT"
