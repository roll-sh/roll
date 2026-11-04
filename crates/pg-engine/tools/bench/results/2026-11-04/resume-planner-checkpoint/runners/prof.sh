#!/bin/bash
# prof.sh <server-label> <workload> <mode> <clients> <secs> <out-name>: pgbench against the
# server a165-<label> (started by hand on its port) while perf records its process with
# frame-pointer call graphs, from a privileged helper container (a165-perf).
set -euo pipefail
s=$1; w=$2; m=$3; c=$4; secs=$5; out=$6
D=$HOME/.cache/a165/bench; P=$HOME/.cache/a165/perf; mkdir -p "$P"
IMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
port=${PORT:-53301}; CC=${CLIENT_CPUS:-12-13,18-19}
docker inspect a165-perf >/dev/null 2>&1 || {
  docker run -d --name a165-perf --privileged --pid=host -v "$HOME/.cache/a165:$HOME/.cache/a165" \
    -v /lib/x86_64-linux-gnu:/lib/x86_64-linux-gnu:ro -v /lib64:/lib64:ro alpine:3.22 sleep infinity >/dev/null
  docker exec a165-perf apk add -q perf
}
pid=$(docker inspect -f '{{.State.Pid}}' "a165-$s")
case $w in select-only) args="-S" ;; simple-update) args="-N" ;; tpcb-like) args="" ;; range) args="-f $D/range.sql" ;; esac
docker run --rm --network host --cpuset-cpus "$CC" -e PGPASSWORD=pw -v "$D:$D" "$IMG" \
  pgbench -h 127.0.0.1 -p "$port" -U ${PGU:-postgres} -n -M "$m" -c "$c" -j "$(( c < 4 ? c : 4 ))" -T $((secs + 4)) $args postgres >"$P/$out.pgbench" 2>&1 &
bp=$!
sleep 2
perf_status=0
bench_status=0
docker exec a165-perf perf record ${PERF_ARGS:--F 999 -g} -p "$pid" -o "$P/$out.data" -- sleep "$secs" >"$P/$out.perf.log" 2>&1 || perf_status=$?
wait "$bp" || bench_status=$?
printf 'perf\t%s\npgbench\t%s\n' "$perf_status" "$bench_status" >"$P/$out.exits.tsv"
test "$perf_status" -eq 0
test "$bench_status" -eq 0
python3 - "$P/$out.pgbench" <<'CHECK'
from pathlib import Path
import re, sys
s = Path(sys.argv[1]).read_text()
n = re.search(r'number of transactions actually processed: (\d+)', s)
f = re.search(r'number of failed transactions: (\d+)', s)
t = re.search(r'^tps = ([0-9.]+)', s, re.M)
assert n and int(n[1]) > 0 and f and int(f[1]) == 0 and t and float(t[1]) > 0, s
assert not re.search(r'\b(?:ERROR|FATAL|PANIC)\b', s), s
CHECK
grep -E '^tps|processed' "$P/$out.pgbench"
