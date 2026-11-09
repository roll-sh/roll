#!/usr/bin/env bash
# roll queues vs RabbitMQ 4: the same PerfTest scenarios against both brokers, each in a
# container with identical CPU and memory limits, one broker at a time. Every run gets a freshly
# started broker, and the brokers take turns (roll, RabbitMQ, roll, ...) so that drift in the
# host's disk or CPU affects both alike. Each scenario runs REPEAT times per broker; the report
# shows the median of each number.
# Writes docs/benchmarks/queues.md.
#
#   scripts/bench-queues.sh            # every scenario, 30 s each
#   DURATION=10 REPEAT=1 scripts/bench-queues.sh
#   CPUS=2 MEM=2g scripts/bench-queues.sh
#   ONLY=5,6 scripts/bench-queues.sh   # re-run some scenarios (by 0-based index); the report reuses
#                                      # the other scenarios' earlier results
#   REPORT_ONLY=1 scripts/bench-queues.sh   # rewrite the report from target/bench-queues
set -euo pipefail
cd "$(dirname "$0")/.."

DURATION=${DURATION:-30}
CPUS=${CPUS:-4}
MEM=${MEM:-4g}
ONLY=${ONLY:-}
REPEAT=${REPEAT:-3}
# Seconds to let the disk settle between scenarios: deleting the previous broker's data keeps
# the (virtual) disk busy for a while and would show up in the next scenario's latency.
SETTLE=${SETTLE:-10}
BROKERS=${BROKERS:-roll rabbitmq}
NET=roll-bench
RABBIT_IMAGE=${RABBIT_IMAGE:-rabbitmq:4-management}
PERFTEST_IMAGE=${PERFTEST_IMAGE:-pivotalrabbitmq/perf-test:latest}
OUT=docs/benchmarks/queues.md
RAW=target/bench-queues
mkdir -p "$RAW" docs/benchmarks

load() { uptime | sed -E 's/.*load averages?: *//; s/,? +/ /g' | cut -d' ' -f1; }

# -v: the image declares a /data volume; drop it with the container.
cleanup() { docker rm -f -v bench-broker >/dev/null 2>&1 || true; }
trap cleanup EXIT

# name | PerfTest arguments
SCENARIOS=(
  "transient, 1 producer → 1 consumer, auto-ack|-x 1 -y 1 -s 1000 -u bench-t -f transient --auto-delete true -a"
  "classic durable, confirms, manual ack|-x 1 -y 1 -s 1000 -u bench-c -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=classic"
  "quorum, confirms, manual ack|-x 1 -y 1 -s 1000 -u bench-q -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=quorum"
  "quorum, 4 producers → 4 consumers|-x 4 -y 4 -s 1000 -u bench-q4 -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=quorum"
  "fan-out to 10 classic queues|-x 1 -y 10 -s 1000 -t fanout -e bench-fan -f persistent -c 200 -q 200"
  "100 queues, 100 producers, 50 msg/s each|-x 100 -y 100 -s 1000 --queue-pattern bench-many-%d --queue-pattern-from 1 --queue-pattern-to 100 -ad false -r 50 -f persistent -c 50 -q 50 --queue-args x-queue-type=quorum"
  "latency at 1000 msg/s (quorum)|-x 1 -y 1 -s 1000 -u bench-lat -ad false -r 1000 -f persistent -c 50 -q 50 --queue-args x-queue-type=quorum"
  "classic durable, consumer acks every 10th message|-x 1 -y 1 -s 1000 -u bench-c10 -ad false -f persistent -c 200 -q 200 --multi-ack-every 10 --queue-args x-queue-type=classic"
)

start_broker() {
  cleanup
  docker run --rm alpine sync >/dev/null 2>&1 || true
  sleep "$SETTLE"
  case $1 in
    roll) docker run -d --name bench-broker --network $NET --cpus "$CPUS" --memory "$MEM" \
            -e ROLL_QUEUES_USER=bench -e ROLL_QUEUES_PASSWORD=bench roll-queues:bench >/dev/null ;;
    rabbitmq) docker run -d --name bench-broker --network $NET --cpus "$CPUS" --memory "$MEM" \
            -e RABBITMQ_DEFAULT_USER=bench -e RABBITMQ_DEFAULT_PASS=bench "$RABBIT_IMAGE" >/dev/null ;;
  esac
  for _ in $(seq 1 120); do
    if docker run --rm --network $NET "$PERFTEST_IMAGE" --uri amqp://bench:bench@bench-broker:5672 -x 0 -y 0 -z 1 >/dev/null 2>&1; then
      return
    fi
    sleep 1
  done
  echo "bench: $1 did not start" >&2
  docker logs bench-broker | tail -20 >&2
  exit 1
}

# Prints "send recv p50 p99" (msg/s, µs).
run() {
  local broker=$1 i=$2 args=$3 log="$RAW/$1-$2-$4.log"
  # shellcheck disable=SC2086
  docker run --rm --network $NET "$PERFTEST_IMAGE" --uri amqp://bench:bench@bench-broker:5672 -z "$DURATION" $args >"$log" 2>&1 || true
  local send recv lat
  send=$(grep -Eo 'sending rate avg: [0-9]+' "$log" | grep -Eo '[0-9]+$' | tail -1 || true)
  recv=$(grep -Eo 'receiving rate avg: [0-9]+' "$log" | grep -Eo '[0-9]+$' | tail -1 || true)
  lat=$(grep -E 'consumer latency min/median/75th/95th/99th' "$log" | tail -1 | grep -Eo '[0-9]+/[0-9]+/[0-9]+/[0-9]+/[0-9]+' || true)
  echo "${send:-0} ${recv:-0} $(echo "${lat:-0/0/0/0/0}" | cut -d/ -f2) $(echo "${lat:-0/0/0/0/0}" | cut -d/ -f5)"
}

# Median of each column over the lines on stdin.
median() {
  awk '{ for (c = 1; c <= NF; c++) v[c, NR] = $c; n = NR; w = NF }
       END { for (c = 1; c <= w; c++) { k = 0; for (r = 1; r <= n; r++) a[++k] = v[c, r]
             for (x = 1; x <= k; x++) for (y = x + 1; y <= k; y++) if (a[y] + 0 < a[x] + 0) { t = a[x]; a[x] = a[y]; a[y] = t }
             printf "%s%s", a[int((k + 1) / 2)], (c < w ? " " : "\n") } }'
}

# Resident memory of the broker container in MiB.
mem_mib() {
  docker stats --no-stream --format '{{.MemUsage}}' bench-broker | awk '{ v = $1; u = v; gsub(/[0-9.]/, "", u); gsub(/[^0-9.]/, "", v)
    print int(u == "GiB" ? v * 1024 : u == "KiB" ? v / 1024 : v) }'
}

if [ -z "${REPORT_ONLY:-}" ]; then
docker info >/dev/null 2>&1 || { echo "bench: Docker is not running" >&2; exit 1; }
docker build -q -f deploy/queues/Dockerfile -t roll-queues:bench . >/dev/null
docker pull -q "$RABBIT_IMAGE" >/dev/null
docker pull -q "$PERFTEST_IMAGE" >/dev/null
docker network inspect $NET >/dev/null 2>&1 || docker network create $NET >/dev/null

[ -n "$ONLY" ] || load > "$RAW/host-load.txt"
for i in "${!SCENARIOS[@]}"; do
  if [ -n "$ONLY" ] && [[ ",$ONLY," != *",$i,"* ]]; then continue; fi
  name=${SCENARIOS[$i]%%|*}
  args=${SCENARIOS[$i]#*|}
  echo "bench: $name"
  # Only the brokers being run: BROKERS=roll keeps RabbitMQ's earlier runs for the report.
  for broker in $BROKERS; do rm -f "$RAW/$broker-$i.runs"; done
  for r in $(seq 1 "$REPEAT"); do
    for broker in $BROKERS; do
      start_broker "$broker"
      run "$broker" "$i" "$args" "$r" >> "$RAW/$broker-$i.runs"
      [ -n "$ONLY" ] || load >> "$RAW/host-load.txt"
      m=$(mem_mib || echo 0)
      peak=$(cat "$RAW/$broker-mem.peak" 2>/dev/null || echo 0)
      [ "$m" -gt "$peak" ] && echo "$m" > "$RAW/$broker-mem.peak"
    done
  done
  for broker in $BROKERS; do
    median < "$RAW/$broker-$i.runs" > "$RAW/$broker-$i.res"
    echo "  $broker: $(cat "$RAW/$broker-$i.res")"
  done
done
if [ -z "$ONLY" ]; then
  for broker in $BROKERS; do mv -f "$RAW/$broker-mem.peak" "$RAW/$broker-mem.txt" 2>/dev/null || true; done
fi
rm -f "$RAW"/*.peak
fi

fmt() { awk -v n="$1" 'BEGIN { if (n >= 1000) printf "%.1fk", n / 1000; else printf "%d", n }'; }
# Received rate, plus the sent rate when the publisher left a backlog behind (over 1% more).
rate() { awk -v s="$1" -v r="$2" 'BEGIN { f = r >= 1000 ? sprintf("%.1fk", r / 1000) : sprintf("%d", r)
  if (s > r * 1.01) f = f sprintf(" (sent %.1fk)", s / 1000); print f }'; }
ms() { awk -v n="$1" 'BEGIN { printf "%.1f", n / 1000 }'; }
# "min–max" of the p99 column over a scenario's runs, in ms.
p99_range() { awk '{ v = $4 / 1000; if (NR == 1 || v < lo) lo = v; if (NR == 1 || v > hi) hi = v } END { if (NR) printf "%.1f–%.1f", lo, hi }' "$1" 2>/dev/null; }

{
  echo "# roll queues vs RabbitMQ 4"
  echo
  echo "PerfTest ($PERFTEST_IMAGE) against each broker in a container limited to $CPUS CPUs and $MEM,"
  echo "${DURATION}s per run, 1 KB messages, run by \`scripts/bench-queues.sh\` on $(uname -sm), $(date -u +%Y-%m-%d)."
  echo "RabbitMQ image: \`$RABBIT_IMAGE\`. Every run starts a fresh broker, the brokers take turns, and each"
  echo "scenario runs $REPEAT times per broker; the table shows the median of each number, and the"
  echo "range of p99 over the runs. Rates are received msg/s (higher is better), with the sent rate"
  echo "when the consumers fell behind; latency is consumer latency in ms (lower is better)."
  echo
  echo "| scenario | roll msg/s | RabbitMQ msg/s | roll p50 / p99 ms | RabbitMQ p50 / p99 ms | p99 range: roll · RabbitMQ |"
  echo "| --- | ---: | ---: | ---: | ---: | ---: |"
  for i in "${!SCENARIOS[@]}"; do
    [ -f "$RAW/roll-$i.res" ] && [ -f "$RAW/rabbitmq-$i.res" ] || continue
    read -r rs rr rp50 rp99 < "$RAW/roll-$i.res"
    read -r qs qr qp50 qp99 < "$RAW/rabbitmq-$i.res"
    echo "| ${SCENARIOS[$i]%%|*} | $(rate "$rs" "$rr") | $(rate "$qs" "$qr") | $(ms "$rp50") / $(ms "$rp99") | $(ms "$qp50") / $(ms "$qp99") | $(p99_range "$RAW/roll-$i.runs") · $(p99_range "$RAW/rabbitmq-$i.runs") |"
  done
  echo
  echo "Single-consumer scenarios at full speed show what happens when the publisher outruns the"
  echo "consumer: the backlog grows for the whole run and latency is its age, not delivery time. Here"
  echo "that is roll's classic queue with a consumer acking every message, and RabbitMQ's quorum queue:"
  echo "the rows that show a sent rate. Prefetch is a promise to the consumer, so neither broker holds"
  echo "deliveries back to batch them. When the consumer acks every 10th message (\`multiple\`, the"
  echo "last scenario), roll's keeps up."
  echo
  if [ -s "$RAW/host-load.txt" ]; then
    echo "The host's 1-minute load average ranged over $(sort -n "$RAW/host-load.txt" | head -1)–$(sort -n "$RAW/host-load.txt" | tail -1) during the run. With"
    echo "other work on the machine, tail latency in the rate-limited scenarios is dominated by the"
    echo "host (fsync jitter of the VM's disk): compare the ranges, not just the medians."
    echo
  fi
  echo "Peak broker memory at the end of a run: roll $(cat "$RAW/roll-mem.txt" 2>/dev/null) MiB, RabbitMQ $(cat "$RAW/rabbitmq-mem.txt" 2>/dev/null) MiB."
  echo
  echo "Raw PerfTest output is in \`target/bench-queues/\`. Scenario arguments:"
  echo
  for s in "${SCENARIOS[@]}"; do echo "- ${s%%|*}: \`${s#*|}\`"; done
} > "$OUT"
echo "bench: wrote $OUT"
