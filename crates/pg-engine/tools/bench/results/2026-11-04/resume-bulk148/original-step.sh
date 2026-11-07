#!/bin/sh
# step.sh <target> <label> <db> <sql>   |   step.sh <target> <label> -- <cmd in a148-cli...>
#   target: engine name (eng-<name>/pid, port in eng-<name>/port) or "pg" (a148-pg's cgroup).
# Runs <sql> through psql in a148-cli and prints: label wall_s cpu_s instr_u(e9) written_MB
A=$HOME/.cache/a148
t=$1; label=$2; db=$3; sql=${4:-}; shift 3
W=$A/work; mkdir -p "$W"
if [ "$t" = pg ]; then
  port=${PG_PORT_T:-52960}
  id=$(docker inspect -f '{{.Id}}' ${PG_NAME_T:-a148-pg})
  CG=/sys/fs/cgroup/system.slice/docker-$id.scope
  cpu() { awk '/^usage_usec/ {printf "%.3f", $2 / 1e6}' "$CG/cpu.stat"; }
  wr() { awk '{for (i = 1; i <= NF; i++) if ($i ~ /^wbytes=/) {sub("wbytes=", "", $i); s += $i}} END {printf "%.0f", s / 1048576}' "$CG/io.stat"; }
else
  pid=$(cat "$A/eng-$t/pid"); port=$(cat "$A/eng-$t/port")
  tck=$(getconf CLK_TCK)
  cpu() { awk -v t="$tck" '{printf "%.3f", ($14 + $15) / t}' "/proc/$pid/stat"; }
  wr() { awk '/^write_bytes/ {printf "%.0f", $2 / 1048576}' "/proc/$pid/io"; }
  rm -f "$W/perf-$t.txt" "$W/perfpid-$t"
  docker exec -d a148-perf sh -c "perf stat -e instructions:u -x, -p $pid -o /w/perf-$t.txt & echo \$! > /w/perfpid-$t; wait"
  i=0; until [ -s "$W/perfpid-$t" ] || [ $i -ge 50 ]; do sleep 0.1; i=$((i + 1)); done
  sleep 0.3
fi
c0=$(cpu); w0=$(wr); s0=$(date +%s.%N)
if [ "$db" = -- ]; then
  docker exec -e PGPASSWORD=pw -e PGPORT="$port" -e PGHOST=127.0.0.1 -e PGUSER=postgres a148-cli nice -n 19 "$@" >"$W/out-$t.txt" 2>&1 || { echo "FAILED $label"; tail -n 3 "$W/out-$t.txt"; }
else
  docker exec -e PGPASSWORD=pw a148-cli nice -n 19 psql -X -q -h 127.0.0.1 -p "$port" -U postgres -d "$db" -v ON_ERROR_STOP=1 -c "$sql" >"$W/out-$t.txt" 2>&1 || { echo "FAILED $label"; tail -n 3 "$W/out-$t.txt"; }
fi
s1=$(date +%s.%N); c1=$(cpu); w1=$(wr)
ins=-
if [ "$t" != pg ]; then
  docker exec a148-perf sh -c "kill -INT \$(cat /w/perfpid-$t)"
  i=0; until grep -q instructions "$W/perf-$t.txt" 2>/dev/null || [ $i -ge 100 ]; do sleep 0.1; i=$((i + 1)); done
  ins=$(awk -F, '/instructions/ {printf "%.3f", $1 / 1e9}' "$W/perf-$t.txt")
fi
awk -v l="$label" -v t="$t" -v s0="$s0" -v s1="$s1" -v c0="$c0" -v c1="$c1" -v w0="$w0" -v w1="$w1" -v i="$ins" \
  'BEGIN {printf "%s\t%s\twall %.3f\tcpu %.3f\tinstr %s\twrote %d\n", t, l, s1 - s0, c1 - c0, i, w1 - w0}'
