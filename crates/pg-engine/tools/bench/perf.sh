#!/bin/sh
# Profile a running engine with perf from a privileged helper container (the host's
# perf_event_paranoid keeps perf from unprivileged users, and the host has no perf).
#
#   tools/bench/perf.sh start            # the helper container (a36-perf)
#   tools/bench/perf.sh record <pid> <secs> [out]   # perf record -g (dwarf) into $BENCH_DATA/perf
#   tools/bench/perf.sh report [out] [args...]      # perf report --stdio
#   tools/bench/perf.sh stop
#
# Build the engine with CARGO_PROFILE_RELEASE_DEBUG=line-tables-only for readable stacks.
set -eu
if [ -z "${BENCH_SG:-}" ] && ! docker info >/dev/null 2>&1; then
  q=""; for a in "$0" "$@"; do q="$q '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"; done
  BENCH_SG=1 exec sg docker -c "BENCH_SG=1 sh $q"
fi
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
DATA=${BENCH_DATA:-$HOME/.cache/a36}
NAME=${PERF_NAME:-a36-perf}
mkdir -p "$DATA/perf"
cmd=$1; shift
case $cmd in
start)
  docker run -d --name "$NAME" --privileged --pid=host \
    -v "$ROOT/target:$ROOT/target:ro" -v "$DATA/perf:$DATA/perf" \
    -v /lib/x86_64-linux-gnu:/lib/x86_64-linux-gnu:ro -v /lib64:/lib64:ro \
    alpine:3.22 sleep infinity >/dev/null
  docker exec "$NAME" apk add -q perf
  ;;
record)
  pid=$1; [ "$pid" = engine ] && pid=$(cat "$DATA/engine.pid"); secs=$2; out=${3:-perf.data}
  docker exec "$NAME" perf record -F 499 -g --call-graph dwarf,16384 -p "$pid" -o "$DATA/perf/$out" -- sleep "$secs" 2>&1 | tail -2
  ;;
flat)
  pid=$1; [ "$pid" = engine ] && pid=$(cat "$DATA/engine.pid"); secs=$2; out=${3:-flat.data}
  docker exec "$NAME" perf record -F 999 -p "$pid" -o "$DATA/perf/$out" -- sleep "$secs" 2>&1 | tail -2
  ;;
report)
  out=${1:-perf.data}; shift || true
  docker exec "$NAME" perf report -i "$DATA/perf/$out" --stdio "$@" 2>/dev/null
  ;;
stop)
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  ;;
*) echo "unknown command $cmd" >&2; exit 2 ;;
esac
