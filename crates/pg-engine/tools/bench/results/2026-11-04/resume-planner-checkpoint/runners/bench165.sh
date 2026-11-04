#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/bench165
mkdir -p "$ev"
git -C "$W" rev-parse HEAD > "$ev/source.txt"
git -C "$W" diff --binary > "$ev/source.patch"
sha256sum /home/meszmate/.cache/a165/scripts/{matrix,conn1000,footprint}.sh "$E/tools/bench/scoreboard.py" > "$ev/harness.sha256"
cp /home/meszmate/.cache/a165/scripts/{matrix,conn1000,footprint}.sh "$ev/"
uptime > "$ev/host.txt"; nproc >> "$ev/host.txt"
run() {
  local name=$1 status=0; shift
  printf '%q ' "$@" > "$ev/$name.command"; printf '\n' >> "$ev/$name.command"
  "$@" > "$ev/$name.log" 2>&1 || status=$?
  printf '%s\t%s\n' "$name" "$status" | tee -a "$ev/exits.tsv"
  test "$status" -eq 0
  test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
}
test -x /home/meszmate/.cache/a165/bin/resume/roll-engine
test ! -s "$ev/exits.tsv"
sha256sum /home/meszmate/.cache/a165/bin/base/roll-engine > "$ev/baseline-before.sha256"
run baseline-build env CARGO_TARGET_DIR=/home/meszmate/.cache/a165/target-base cargo build --manifest-path /home/meszmate/.cache/a165/base-src/Cargo.toml --profile dist -p pg-engine --bin roll-engine
cmp /home/meszmate/.cache/a165/target-base/dist/roll-engine /home/meszmate/.cache/a165/bin/base/roll-engine
sha256sum --check "$ev/baseline-before.sha256"
sha256sum /home/meszmate/.cache/a165/bin/{base,resume}/roll-engine /home/meszmate/.cache/a165/bin/bench > "$ev/binaries.sha256"
export SERVER_CPUS=8-11 CLIENT_CPUS=12-13,18-19
run footprint env BENCH_BIN=/home/meszmate/.cache/a165/bin/resume BENCH_CLIENT=/home/meszmate/.cache/a165/bin/bench SB_DATA="$ev/footprint" PG_NAME=a165-resume-gate-pg ENGINE_NAME=a165-resume-gate-engine PG_PORT=53304 ENGINE_PORT=53305 SB_ONLY=none ROUNDS=1 IDLE_SECS=60 "$E/tools/bench/scoreboard.sh" quick
run scaled bash /home/meszmate/.cache/a165/scripts/conn1000.sh resume 1000
run matrix env MATRIX_DATA="$ev/matrix-data" bash /home/meszmate/.cache/a165/scripts/matrix.sh "$ev/matrix.tsv" 2 pg base resume
test "$(wc -l < "$ev/matrix.tsv.raw/validation.tsv")" -eq 6
sha256sum --check "$ev/binaries.sha256"
sha256sum --check "$ev/harness.sha256"
git -C "$W" diff --binary > "$ev/final.patch"
cmp "$ev/source.patch" "$ev/final.patch"
echo BENCH165-PASS
