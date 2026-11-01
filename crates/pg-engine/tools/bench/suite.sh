#!/bin/bash
# The whole benchmark suite against one target, results appended to $BENCH_DATA/results/<label>.tsv
# (the RESULT lines) and the pgbench summaries.
#
#   tools/bench/suite.sh pg              # PostgreSQL (tools/bench/run.sh pg-start first)
#   tools/bench/suite.sh engine [label]  # the engine server (BENCH_BIN picks the binaries)
#   tools/bench/suite.sh inproc [label]  # the engine in-process, over the data the engine run left
#
# SUITE selects parts: pgbench,oltp,analytic,tpch,bulk,ddl,idle (default: all but tpch).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
DATA=${BENCH_DATA:-$HOME/.cache/a36}
t=$1; label=${2:-$1}
OUT=$DATA/results/$label.tsv
mkdir -p "$DATA/results" "$DATA/logs"
SUITE=${SUITE:-pgbench,oltp,analytic,bulk,ddl,idle}
SECS=${SECS:-15}
has() { case ",$SUITE," in *",$1,"*) return 0 ;; esac; return 1; }
run() {
  "$HERE/run.sh" "$@" 2>&1 | tee -a "$DATA/logs/suite-$label.log" | awk -F '\t' '
    /^RESULT\t/ {
      seen = 1
      print
      for (i = 5; i <= NF; i++) {
        if ($i ~ /^error=/ || ($i ~ /^errors=/ && $i != "errors=0")) bad = 1
      }
    }
    END { if (!seen || bad) exit 1 }
  ' >>"$OUT"
}
bench() {
  if [ "$t" = inproc ]; then run inproc --label "$label" "$@"; else run bench "$t" --label "$label" "$@"; fi
}
pgb() {
  # pgbench <scale> <mode> <clients>: tps and latency, as a RESULT line.
  s=$1; mode=$2; c=$3; j=$c; [ "$j" -gt 4 ] && j=4
  flag=""; [ "$mode" = select ] && flag=-S
  r=$("$HERE/run.sh" pgbench "$t" -M prepared $flag -c "$c" -j "$j" -T "$SECS" bench 2>&1 | tee -a "$DATA/logs/suite-$label.log")
  tps=$(echo "$r" | sed -n 's/^tps = \([0-9.]*\).*/\1/p')
  lat=$(echo "$r" | sed -n 's/^latency average = \([0-9.]*\) ms/\1/p')
  failed=$(echo "$r" | sed -n 's/^number of failed transactions: \([0-9]*\).*/\1/p')
  if ! awk -v tps="$tps" -v lat="$lat" -v failed="$failed" 'BEGIN {
    exit !(tps ~ /^[0-9]+([.][0-9]+)?$/ && tps > 0 &&
           lat ~ /^[0-9]+([.][0-9]+)?$/ && failed == "0")
  }'; then
    printf 'invalid pgbench result; see %s\n' "$DATA/logs/suite-$label.log" >&2
    return 1
  fi
  printf 'RESULT\t%s\tpgbench\ts%s_%s\tclients=%s\ttps=%s\tlat_ms=%s\tfailed=%s\n' \
    "$label" "$s" "$mode" "$c" "$tps" "$lat" "$failed" >>"$OUT"
}
if [ "$t" != inproc ]; then
  "$HERE/run.sh" bench "$t" --label "$label" createdb --db bench >/dev/null 2>&1
fi
if has pgbench && [ "$t" != inproc ]; then
  for s in 1 10; do
    r=$("$HERE/run.sh" pgbench "$t" -i -I dtGvp -s "$s" bench 2>&1 | tee -a "$DATA/logs/suite-$label.log" | tail -1)
    printf 'RESULT\t%s\tpgbench\tinit_s%s\t%s\n' "$label" "$s" "$(echo "$r" | tr '\t' ' ')" >>"$OUT"
    for mode in select tpcb; do
      for c in 1 8 32; do pgb "$s" "$mode" "$c"; done
    done
  done
fi
if has oltp; then
  bench oltp-load --rows 1000000
  bench oltp --rows 1000000 --secs 8 --clients 1,8
fi
if has analytic; then
  for rows in 200000 2000000; do
    bench analytic-load --rows "$rows" --label "$label-$rows"
    bench analytic --rows "$rows" --reps 3 --timeout 600 --label "$label-$rows"
  done
fi
if has tpch; then
  if [ "$t" != inproc ]; then
    "$HERE/run.sh" bench "$t" createdb --db tpch >/dev/null 2>&1
    bench tpch-load --sf "${SF:-1}" --db tpch
  fi
  bench tpch --reps "${TPCH_REPS:-3}" --timeout 900 --db tpch
fi
if has bulk; then
  bench bulk --rows 100000 --reps 3 --label "$label-100k"
  bench bulk --rows 1000000 --reps 2 --label "$label-1m"
fi
if has ddl; then
  bench ddl --tables 500 --reps 20
fi
if has idle && [ "$t" != inproc ]; then
  bench idle --sessions 100 --dbs 10
fi
echo "suite $label done"
