#!/usr/bin/env bash
# roll queues' log storage on the fleet: publish-confirm latency and throughput of a queues node
# whose logs are objects in S3 (group commit, what a roll cloud node uses), next to local disk.
# PerfTest scenarios against one broker in a container, its storage being:
#
#   file        local disk with the shared WAL (a node alone; the baseline)
#   s3-local    MinIO on the same Docker network (the code path, with ~1 ms PUTs)
#   s3-modelled MinIO behind Toxiproxy adding S3 Standard's latency to every answer
#               (LATENCY ± JITTER ms; 25 ± 10 by default)
#   s3          a real S3 bucket: set ROLL_BENCH_S3_BUCKET and AWS credentials (AWS_REGION,
#               AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY[, AWS_SESSION_TOKEN]); the broker runs here,
#               so run this on an EC2 instance in that region for numbers that mean something.
#
# Writes docs/benchmarks/queues-fleet.md.
#
#   scripts/bench-queues-fleet.sh                  # file, s3-local, s3-modelled (s3 too when set)
#   MODES="s3-modelled" DURATION=20 scripts/bench-queues-fleet.sh
#   LATENCY=40 JITTER=20 scripts/bench-queues-fleet.sh
#   REPORT_ONLY=1 scripts/bench-queues-fleet.sh   # rewrite the report from target/bench-queues-fleet
set -euo pipefail
cd "$(dirname "$0")/.."

DURATION=${DURATION:-30}
CPUS=${CPUS:-4}
MEM=${MEM:-4g}
LATENCY=${LATENCY:-25}
JITTER=${JITTER:-10}
MODES=${MODES:-file s3-local s3-modelled${ROLL_BENCH_S3_BUCKET:+ s3}}
NET=roll-bench-fleet
PERFTEST_IMAGE=${PERFTEST_IMAGE:-pivotalrabbitmq/perf-test:latest}
MINIO_IMAGE=${MINIO_IMAGE:-cgr.dev/chainguard/minio}
TOXIPROXY_IMAGE=${TOXIPROXY_IMAGE:-ghcr.io/shopify/toxiproxy:2.9.0}
MC_IMAGE=${MC_IMAGE:-cgr.dev/chainguard/minio-client}
OUT=docs/benchmarks/queues-fleet.md
RAW=target/bench-queues-fleet
mkdir -p "$RAW" docs/benchmarks

# name | PerfTest arguments (quorum queues: they deliver only what's durable)
SCENARIOS=(
  "1 producer → 1 consumer, confirms (200 in flight)|-x 1 -y 1 -s 1000 -u bench-q -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=quorum"
  "4 producers → 4 consumers, confirms (200 in flight each)|-x 4 -y 4 -s 1000 -u bench-q4 -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=quorum"
  "1000 msg/s, confirms (50 in flight)|-x 1 -y 1 -s 1000 -u bench-lat -ad false -r 1000 -f persistent -c 50 -q 50 --queue-args x-queue-type=quorum"
  "100 queues, 100 producers × 50 msg/s|-x 100 -y 100 -s 1000 --queue-pattern bench-many-%d --queue-pattern-from 1 --queue-pattern-to 100 -ad false -r 50 -f persistent -c 50 -q 50 --queue-args x-queue-type=quorum"
)

cleanup() { docker rm -f -v bench-fleet-broker bench-fleet-minio bench-fleet-toxi >/dev/null 2>&1 || true; }
trap cleanup EXIT

start_s3() {
  docker run -d --name bench-fleet-minio --network $NET -e MINIO_ROOT_USER=bench -e MINIO_ROOT_PASSWORD=bench-secret \
    "$MINIO_IMAGE" server /data >/dev/null
  for _ in $(seq 1 60); do
    docker run --rm --network container:bench-fleet-minio -e MC_HOST_local=http://bench:bench-secret@127.0.0.1:9000 "$MC_IMAGE" \
      mb --ignore-existing local/roll >/dev/null 2>&1 && break
    sleep 1
  done
  docker run -d --name bench-fleet-toxi --network $NET "$TOXIPROXY_IMAGE" >/dev/null
  sleep 1
  docker exec bench-fleet-toxi /toxiproxy-cli create -l 0.0.0.0:9001 -u bench-fleet-minio:9000 s3 >/dev/null
  docker exec bench-fleet-toxi /toxiproxy-cli toxic add -t latency -a latency="$LATENCY" -a jitter="$JITTER" s3 >/dev/null
}

start_broker() {
  docker rm -f -v bench-fleet-broker >/dev/null 2>&1 || true
  local env=(-e ROLL_QUEUES_USER=bench -e ROLL_QUEUES_PASSWORD=bench -e ROLL_ADMIN_TOKEN=bench-operator-token-0123456789abcdef)
  case $1 in
    file) ;;
    s3-local) env+=(-e ROLL_QUEUES_STORAGE=s3:roll/logs -e ROLL_S3_BUCKET=roll -e ROLL_S3_ENDPOINT=http://bench-fleet-minio:9000 -e AWS_REGION=us-east-1
                    -e AWS_ACCESS_KEY_ID=bench -e AWS_SECRET_ACCESS_KEY=bench-secret) ;;
    s3-modelled) env+=(-e ROLL_QUEUES_STORAGE=s3:roll/logs -e ROLL_S3_BUCKET=roll -e ROLL_S3_ENDPOINT=http://bench-fleet-toxi:9001 -e AWS_REGION=us-east-1
                       -e AWS_ACCESS_KEY_ID=bench -e AWS_SECRET_ACCESS_KEY=bench-secret) ;;
    s3) env+=(-e "ROLL_QUEUES_STORAGE=s3:$ROLL_BENCH_S3_BUCKET/roll-bench-$(date +%s)" -e AWS_REGION -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_SESSION_TOKEN) ;;
  esac
  docker run -d --name bench-fleet-broker --network $NET --cpus "$CPUS" --memory "$MEM" "${env[@]}" roll-queues:bench >/dev/null
  for _ in $(seq 1 120); do
    if docker run --rm --network $NET "$PERFTEST_IMAGE" --uri amqp://bench:bench@bench-fleet-broker:5672 -x 0 -y 0 -z 1 >/dev/null 2>&1; then
      return
    fi
    sleep 1
  done
  echo "bench: the broker ($1) did not start" >&2
  docker logs bench-fleet-broker | tail -20 >&2
  exit 1
}

# Prints "sent recv confirm_p50 confirm_p99 consumer_p50 consumer_p99" (msg/s, µs).
run() {
  local log="$RAW/$1-$2.log"
  # shellcheck disable=SC2086
  docker run --rm --network $NET "$PERFTEST_IMAGE" --uri amqp://bench:bench@bench-fleet-broker:5672 -z "$DURATION" $3 >"$log" 2>&1 || true
  local send recv conf cons
  send=$(grep -Eo 'sending rate avg: [0-9]+' "$log" | grep -Eo '[0-9]+$' | tail -1 || true)
  recv=$(grep -Eo 'receiving rate avg: [0-9]+' "$log" | grep -Eo '[0-9]+$' | tail -1 || true)
  conf=$(grep -E 'confirm latency min/median/75th/95th/99th' "$log" | tail -1 | grep -Eo '[0-9]+/[0-9]+/[0-9]+/[0-9]+/[0-9]+' || true)
  cons=$(grep -E 'consumer latency min/median/75th/95th/99th' "$log" | tail -1 | grep -Eo '[0-9]+/[0-9]+/[0-9]+/[0-9]+/[0-9]+' || true)
  conf=${conf:-0/0/0/0/0} cons=${cons:-0/0/0/0/0}
  echo "${send:-0} ${recv:-0} $(cut -d/ -f2 <<<"$conf") $(cut -d/ -f5 <<<"$conf") $(cut -d/ -f2 <<<"$cons") $(cut -d/ -f5 <<<"$cons")"
}

if [ -z "${REPORT_ONLY:-}" ]; then
docker info >/dev/null 2>&1 || { echo "bench: Docker is not running" >&2; exit 1; }
docker build -q -f deploy/queues/Dockerfile -t roll-queues:bench . >/dev/null
docker pull -q "$PERFTEST_IMAGE" >/dev/null
docker network inspect $NET >/dev/null 2>&1 || docker network create $NET >/dev/null
cleanup
start_s3
for mode in $MODES; do
  for i in "${!SCENARIOS[@]}"; do
    echo "bench: $mode: ${SCENARIOS[$i]%%|*}"
    start_broker "$mode"
    run "$mode" "$i" "${SCENARIOS[$i]#*|}" > "$RAW/$mode-$i.res"
    echo "  $(cat "$RAW/$mode-$i.res")"
  done
done
fi

rate() { awk -v s="$1" -v r="$2" 'BEGIN { f = r >= 1000 ? sprintf("%.1fk", r / 1000) : sprintf("%d", r)
  if (s > r * 1.01) f = f sprintf(" (sent %.1fk)", s / 1000); print f }'; }
ms() { awk -v n="$1" 'BEGIN { printf "%.1f", n / 1000 }'; }
{
  echo "# roll queues on the fleet: logs in S3"
  echo
  echo "PerfTest (\`$PERFTEST_IMAGE\`) against one roll queues node in a container limited to $CPUS CPUs"
  echo "and $MEM, ${DURATION}s per scenario, 1 KB persistent messages to quorum queues (which deliver only"
  echo "what's durable), run by \`scripts/bench-queues-fleet.sh\` on $(uname -sm), $(date -u +%Y-%m-%d)."
  echo "Rates are received msg/s; latencies are PerfTest's confirm latency (publish → confirm) and"
  echo "consumer latency (publish → delivery), p50 / p99 in ms."
  echo
  echo "| storage | scenario | msg/s | confirm p50 / p99 | consumer p50 / p99 |"
  echo "| --- | --- | ---: | ---: | ---: |"
  for mode in $MODES; do
    for i in "${!SCENARIOS[@]}"; do
      [ -f "$RAW/$mode-$i.res" ] || continue
      read -r s r cp50 cp99 lp50 lp99 < "$RAW/$mode-$i.res"
      echo "| $mode | ${SCENARIOS[$i]%%|*} | $(rate "$s" "$r") | $(ms "$cp50") / $(ms "$cp99") | $(ms "$lp50") / $(ms "$lp99") |"
    done
  done
  echo
  echo "- **file**: local disk, group-committed through the shared WAL: a node alone (\`rolld\` on a laptop)."
  echo "- **s3-local**: the fleet's code path (\`s3:\` storage, group commit) on MinIO in the same Docker"
  echo "  network, no added network latency: what the storage code itself costs."
  echo "- **s3-modelled**: the same through Toxiproxy, which delays every answer by ${LATENCY} ± ${JITTER} ms,"
  echo "  standing in for S3 Standard's PUT latency in-region. A model, not a measurement of S3."
  echo "- **s3**: a real S3 Standard bucket (only when \`ROLL_BENCH_S3_BUCKET\` was set)."
  echo
  echo "## Reading the numbers"
  echo
  echo "- **One queue's throughput is its confirm window over the confirm latency.** A publisher with"
  echo "  N confirms in flight gets at most N / latency messages a second: 200 in flight at ~75 ms is"
  echo "  ~2.7k/s, and the 1000 msg/s scenario (50 in flight) can't reach its rate on modelled S3."
  echo "  Publishers that want more from one queue keep more in flight or use more queues."
  echo "- **A confirm costs about three PUT latencies** on the object log: a queue has one log append"
  echo "  in flight at a time (the object log has no pipelined appends), and a node's shared stream"
  echo "  has one PUT in flight at a time, so a message waits for the PUT before it, then its own."
  echo "  Pipelining both would bring it towards one PUT."
  echo "- **A node's throughput isn't capped by PUTs**: with many queues every stream PUT carries"
  echo "  whatever arrived meanwhile (the 100-queue scenario keeps its 5k msg/s at any latency)."
  echo "- **s3-local** is bounded by MinIO itself (it syncs every object to disk) on a shared host, not"
  echo "  by the network; read it as the code path's cost, not as S3's."
  echo "- Why S3 Standard is still the fleet's default (over keepers on the nodes, 1–3 ms): docs/queues/storage.md,"
  echo "  \"On roll cloud\"."
  echo
  echo "Raw PerfTest output is in \`target/bench-queues-fleet/\`. Scenario arguments:"
  echo
  for s in "${SCENARIOS[@]}"; do echo "- ${s%%|*}: \`${s#*|}\`"; done
} > "$OUT"
echo "bench: wrote $OUT"
