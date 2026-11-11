#!/bin/bash
# The lift table: lift.sh's checks for PostgreSQL, the engine at its 400,000-version limit and
# the engine with the limit lifted (ROLL_MODEL_MAX_VERSIONS), in $ROUNDS interleaved rounds
# (the order rotates each round), then one row per check: CPU seconds (per transaction for
# the pgbench checks, in ms), the statements' times, peak memory. Lift the limit only when
# every lifted row is at or below the 400k column (BENCH.md "Size models, round 5").
#
#   BENCH_BIN=<dist dir> tools/bench/sizing/liftcmp.sh <out dir> [checks...]
#
# Knobs: ROUNDS (2), LIFT (1099511627776), BASE_LIMIT (empty: the built-in limit), and
# lift.sh's (SERVER_CPUS, CLIENT_CPUS, PG_PORT, ENGINE_PORT, PG_NAME, SCALE, CLIENTS, SECS).
set -uo pipefail
OUT=${1:?out dir}; shift
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"
ROUNDS=${ROUNDS:-2}; LIFT=${LIFT:-1099511627776}
export BENCH_DATA=${BENCH_DATA:-$OUT/data}
: >"$OUT/lift.tsv"
for r in $(seq 1 "$ROUNDS"); do
  order="pg base lifted"
  case $((r % 3)) in 2) order="lifted pg base" ;; 0) order="base lifted pg" ;; esac
  for o in $order; do
    case $o in
    pg) ENGINE_ENV= bash "$HERE/lift.sh" pg pg "$@" ;;
    base) ENGINE_ENV=${BASE_LIMIT:+ROLL_MODEL_MAX_VERSIONS=$BASE_LIMIT} bash "$HERE/lift.sh" 400k engine "$@" ;;
    lifted) ENGINE_ENV=ROLL_MODEL_MAX_VERSIONS=$LIFT bash "$HERE/lift.sh" lifted engine "$@" ;;
    esac | sed "s/^LIFT/LIFT\tr$r/" | tee -a "$OUT/lift.tsv"
  done
done
# One row per check: each label's values over the rounds (CPU ms per transaction where the
# check counted transactions, else CPU s; ms for the timed statements; peak MB).
awk -F'\t' '
  { key = $5; lab = $4; v = ""
    for (i = 6; i <= NF; i++) { split($i, kv, "="); f[kv[1]] = kv[2] }
    if ("ms" in f) v = f["ms"] " ms"
    else if (f["work"] != "") v = sprintf("%.3f ms/txn %s MB", f["cpu_s"] * 1000 / f["work"], f["peak_mb"])
    else v = sprintf("%s s %s MB", f["cpu_s"], f["peak_mb"])
    delete f
    if (!(key in seen)) { seen[key] = 1; order[++n] = key }
    cell[key, lab] = (cell[key, lab] == "" ? "" : cell[key, lab] " / ") v }
  END { print "| check | 400k | lifted | PostgreSQL |"; print "|---|---|---|---|"
    for (i = 1; i <= n; i++) { k = order[i]; printf "| %s | %s | %s | %s |\n", k, cell[k, "400k"], cell[k, "lifted"], cell[k, "pg"] } }
' "$OUT/lift.tsv" | tee "$OUT/table.md"
