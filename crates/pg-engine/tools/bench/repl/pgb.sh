#!/bin/sh
# pgbench check of a batch: select-only and tpcb-like CPU per transaction and peak RSS of the
# engine, base and head interleaved (BENCH.md "Replication, round 5").
#
#   A=a152 PORT0=53040 ENGINE=<head> BASE=<base> ROUNDS=4 tools/heavy.sh timeout 7200 sh tools/bench/repl/pgb.sh
#
# Scale 10, -M prepared, 8 clients, 20 s per run, --cache-mb 2048; each run on a fresh engine
# (VmHWM is the process's peak over its pgbench -i and both runs).
set -eu
. "$(dirname "$0")/lib.sh"
ROUNDS=${ROUNDS:-4}
DUR=${DUR:-20}
SCALE=${SCALE:-10}
P=$((PORT0 + 5))
cli_up
r=0
while [ $r -lt "$ROUNDS" ]; do
  r=$((r + 1))
  order="base head"; [ $((r % 2)) -eq 0 ] && order="head base"
  for b in $order; do
    bin=$ENGINE; [ "$b" = base ] && bin=${BASE:?set BASE}
    ENGINE=$bin eng_up "$A-pgb" $P --cache-mb 2048
    pid=$(cat "$WORK/$A-pgb.pid")
    pgbench -p $P -i -s "$SCALE" -q postgres > "$WORK/pgb-round-$r-$b-init.log" 2>&1
    out=""
    for mode in select tpcb; do
      mode_arg=""; [ "$mode" = select ] && mode_arg=-S
      bench_log="$WORK/pgb-round-$r-$b-$mode.log"
      c0=$(cpu_pid "$pid")
      pgbench -p $P -n -M prepared -c 8 -j 4 -T "$DUR" $mode_arg postgres > "$bench_log" 2>&1
      check_pgbench "$bench_log"
      n=$(awk '/number of transactions actually processed/ {print $NF}' "$bench_log" | cut -d/ -f1)
      c1=$(cpu_pid "$pid")
      out="$out $(awk -v a="$c1" -v b="$c0" -v n="$n" 'BEGIN {printf "%.3f ms (%d txns)", (a - b) * 1000 / n, n}')"
    done
    hwm=$(awk '/^VmHWM/ {print int($2 / 1024)}' "/proc/$pid/status")
    printf 'round %s %s select-only/tpcb-like CPU per txn:%s VmHWM %s MB\n' "$r" "$b" "$out" "$hwm"
    eng_down "$A-pgb"
  done
done
