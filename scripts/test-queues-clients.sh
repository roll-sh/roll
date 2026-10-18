#!/usr/bin/env bash
# roll queues compatibility matrix: real client libraries for every protocol (AMQP 0-9-1 and 1.0,
# MQTT, STOMP, streams), RabbitMQ PerfTest and the client libraries' own test suites, all in Docker
# against `rolld --queues-only` (or real RabbitMQ 4 with --rabbitmq).
#
#   scripts/test-queues-clients.sh                 # every client in tests/queues/clients
#   scripts/test-queues-clients.sh python-pika go-amqp091
#   scripts/test-queues-clients.sh --perf          # PerfTest runs only
#   scripts/test-queues-clients.sh --suites        # upstream test suites only
#   scripts/test-queues-clients.sh --all           # clients + PerfTest + suites
#
# Options: --port N (broker port, default random), --url URL (test an already running broker
# instead of building one; must be reachable from containers), --rabbitmq (run everything against
# real RabbitMQ 4 instead, to validate the test programs themselves), --keep (leave the broker
# running).
# Results (logs, summary.md) go to $CARGO_TARGET_DIR/queues-clients (default target/).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

run_clients=0 run_perf=0 run_suites=0 keep=0 port=0 url="" rabbitmq=0 only=()
while (($#)); do
  case "$1" in
    --perf) run_perf=1 ;;
    --suites) run_suites=1 ;;
    --all) run_clients=1 run_perf=1 run_suites=1 ;;
    --port) port=$2; shift ;;
    --url) url=$2; shift ;;
    --keep) keep=1 ;;
    --rabbitmq) rabbitmq=1 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) only+=("$1"); run_clients=1 ;;
  esac
  shift
done
((run_clients + run_perf + run_suites)) || run_clients=1

if ! docker info >/dev/null 2>&1; then
  echo "queues-clients: Docker is not running" >&2
  exit 1
fi

TARGET=${CARGO_TARGET_DIR:-target}
OUT=$TARGET/queues-clients
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
rm -f "$OUT"/*.log "$OUT"/summary.md

# ── broker ──
# Every protocol gets its own port: AMQP 0-9-1 and 1.0 share one, then MQTT, STOMP, streams and
# the management API. --port fixes the AMQP one; the others are free ports picked here.
free_ports() { python3 -c 'import socket
ss = [socket.socket() for _ in range(int(__import__("sys").argv[1]))]
for s in ss: s.bind(("", 0))
print(" ".join(str(s.getsockname()[1]) for s in ss))' "$1"; }
read -r p_amqp mqtt_port stomp_port stream_port mgmt_port http_port api_port <<<"$(free_ports 7)"
((port)) && p_amqp=$port
host=host.docker.internal
if ((rabbitmq)) && [[ -z $url ]]; then
  docker rm -f -v roll-qtest-rabbitmq >/dev/null 2>&1 || true
  # Streams tell clients where to connect, so the advertised port must be the published one.
  printf 'stream.advertised_host = %s\nstream.advertised_port = %s\n' "$host" "$stream_port" >"$OUT/30-stream.conf"
  docker run -d --name roll-qtest-rabbitmq --add-host=host.docker.internal:host-gateway \
    -p "$p_amqp:5672" -p "$mqtt_port:1883" -p "$stomp_port:61613" -p "$stream_port:5552" -p "$mgmt_port:15672" \
    -v "$ROOT/tests/queues/rabbitmq/20-roll.conf:/etc/rabbitmq/conf.d/20-roll.conf:ro" \
    -v "$OUT/30-stream.conf:/etc/rabbitmq/conf.d/30-stream.conf:ro" \
    -v "$ROOT/tests/queues/rabbitmq/enabled_plugins:/etc/rabbitmq/enabled_plugins:ro" rabbitmq:4-management >/dev/null
  ((keep)) || trap 'docker rm -f -v roll-qtest-rabbitmq >/dev/null 2>&1 || true' EXIT
  until docker logs roll-qtest-rabbitmq 2>&1 | grep -q 'Server startup complete'; do sleep 1; done
  port=$p_amqp
  url="amqp://guest:guest@$host:$port/%2F"
  echo "rabbitmq: amqp $port, mqtt $mqtt_port, stomp $stomp_port, stream $stream_port, management $mgmt_port"
elif [[ -z $url ]]; then
  # The real daemon, queues only, with every listener.
  cargo build --release -p rolld
  home=$(mktemp -d "${TMPDIR:-/tmp}/roll-qtest.XXXXXX")
  any=0.0.0.0
  # Debug for mq_amqp logs why a connection or channel ended, which is what a flaky client needs.
  RUST_LOG=${RUST_LOG:-info,mq_amqp=debug} "$TARGET/release/rolld" --home "$home" --queues-only \
    --api "127.0.0.1:$api_port" --public-host "$host" --queues-advertise-host "$host" \
    --queues-amqp "$any:$p_amqp" --queues-mqtt "$any:$mqtt_port" --queues-stomp "$any:$stomp_port" \
    --queues-stream "$any:$stream_port" --queues-management "$any:$mgmt_port" --queues-http "$any:$http_port" \
    --queues-web-mqtt 127.0.0.1:0 --queues-web-stomp 127.0.0.1:0 --queues-rpc 127.0.0.1:0 >"$OUT/broker.log" 2>&1 &
  BROKER_PID=$!
  ((keep)) || trap 'kill $BROKER_PID 2>/dev/null || true; wait $BROKER_PID 2>/dev/null || true; rm -rf "$home"' EXIT
  for _ in $(seq 200); do
    curl -sf -u guest:guest "http://127.0.0.1:$mgmt_port/api/overview" >/dev/null && break
    kill -0 $BROKER_PID 2>/dev/null || { cat "$OUT/broker.log" >&2; exit 1; }
    sleep 0.1
  done
  port=$p_amqp
  url="amqp://guest:guest@$host:$port/%2F"
  echo "broker: rolld pid $BROKER_PID, amqp $port, mqtt $mqtt_port, stomp $stomp_port, stream $stream_port, management $mgmt_port, log $OUT/broker.log"
else
  hostport=${url#*@}; hostport=${hostport%%/*}
  host=${hostport%:*}; port=${hostport##*:}
fi
# A hung client must not hang the matrix (GNU timeout; gtimeout on macOS with coreutils).
TIMEOUT=()
for t in timeout gtimeout; do command -v $t >/dev/null && { TIMEOUT=($t 900); break; }; done
DOCKER_RUN=(docker run --rm --add-host=host.docker.internal:host-gateway
            -e "AMQP_URL=$url" -e "AMQP_HOST=$host" -e "AMQP_PORT=$port" -e "MQTT_PORT=$mqtt_port"
            -e "STOMP_PORT=$stomp_port" -e "STREAM_PORT=$stream_port" -e "MGMT_URL=http://$host:$mgmt_port")
failed=0

# ── clients ──
if ((run_clients)); then
  clients=("${only[@]}")
  if ((${#clients[@]} == 0)); then
    for d in tests/queues/clients/*/; do clients+=("$(basename "$d")"); done
  fi
  for c in "${clients[@]}"; do
    dir=tests/queues/clients/$c
    [[ -f $dir/Dockerfile ]] || { echo "no client $c" >&2; failed=1; continue; }
    echo "── $c"
    if ! docker build -q -t "roll-qtest-$c" "$dir" >"$OUT/$c.build.log" 2>&1; then
      cat "$OUT/$c.build.log" >&2
      echo "FAIL build: docker build failed" >"$OUT/$c.log"
      failed=1
      continue
    fi
    rm -f "$OUT/$c.retry.log" "$OUT/$c.retry.stderr.log"
    if ! ${TIMEOUT[@]+"${TIMEOUT[@]}"} "${DOCKER_RUN[@]}" "roll-qtest-$c" 2>"$OUT/$c.stderr.log" | tee "$OUT/$c.log"; then
      # Docker Desktop's host-gateway proxy now and then stalls a TCP connect under load. Run
      # the failed scenarios once more; the summary marks them "PASS (retry)", so flakes stay visible.
      again=$(sed -n 's/^FAIL \([A-Za-z0-9_.-]*\).*/\1/p' "$OUT/$c.log" | sort -u)
      if [[ -z $again ]]; then
        failed=1
      else
        echo "── $c: retrying ${again//$'\n'/ }"
        # shellcheck disable=SC2086 # one argument per scenario
        if ! ${TIMEOUT[@]+"${TIMEOUT[@]}"} "${DOCKER_RUN[@]}" "roll-qtest-$c" $again 2>"$OUT/$c.retry.stderr.log" | tee "$OUT/$c.retry.log"; then
          failed=1
        fi
      fi
    fi
  done
  python3 - "$OUT" "${clients[@]}" >"$OUT/summary.md" <<'EOF'
import re, sys
out, clients = sys.argv[1], sys.argv[2:]
results, order = {}, []
for c in clients:
    for retry, path in ((False, f"{out}/{c}.log"), (True, f"{out}/{c}.retry.log")):
        try:
            lines = open(path).read().splitlines()
        except FileNotFoundError:
            continue
        for line in lines:
            m = re.match(r"(PASS|FAIL|SKIP) ([\w.-]+):?\s*(.*)", line)
            if m:
                status, name, _ = m.groups()
                if name not in order:
                    order.append(name)
                results[(c, name)] = f"{status} (retry)" if retry else status
print("| scenario | " + " | ".join(clients) + " |")
print("|---|" + "---|" * len(clients))
for name in order:
    print(f"| {name} | " + " | ".join(results.get((c, name), "–") for c in clients) + " |")
EOF
  echo
  cat "$OUT/summary.md"
fi

# ── PerfTest ──
if ((run_perf)); then
  perf() {
    local name=$1; shift
    echo "── perf-test $name: $*"
    docker run --rm --add-host=host.docker.internal:host-gateway pivotalrabbitmq/perf-test:latest \
      --uri "$url" "$@" >"$OUT/perf-$name.log" 2>&1 || failed=1
    local res
    res=$(grep -E 'rate avg|latency min' "$OUT/perf-$name.log" | sed 's/^id: [^,]*, /  /' || true)
    [[ -n $res ]] || { tail -20 "$OUT/perf-$name.log"; failed=1; return; }
    printf 'perf-test %s: %s\n%s\n' "$name" "$*" "$res" | tee -a "$OUT/summary.md"
  }
  perf quick -x 2 -y 2 -u perf-quick -a --time 15
  perf confirms -x 1 -y 1 -u perf-confirms -c 100 -f persistent --time 15
  perf quorum --quorum-queue --queue perf-qq --time 15
  perf quorum-rate --quorum-queue --queue perf-qq-rate -r 10000 --time 15
fi

# ── upstream test suites ──
if ((run_suites)); then
  for s in tests/queues/suites/*/run.sh; do
    name=$(basename "$(dirname "$s")")
    echo "── suite $name"
    if ! "$s" "$host" "$port" 2>&1 | tee "$OUT/suite-$name.log" | grep -E '^(SUITE|FAIL|ERROR)'; then
      :
    fi
    grep "^SUITE $name " "$OUT/suite-$name.log" >>"$OUT/summary.md" || echo "SUITE $name did not finish" >>"$OUT/summary.md"
    grep -q "^SUITE $name .*failed=0" "$OUT/suite-$name.log" || failed=1
  done
fi

echo "results: $OUT/summary.md"
exit $failed
