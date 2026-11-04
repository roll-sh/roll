#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/jobA-repaired
mkdir -p "$ev"
git -C "$W" rev-parse HEAD > "$ev/source.txt"
run() {
  local name=$1 status=0; shift
  "$@" > "$ev/$name.log" 2>&1 || status=$?
  printf '%s\t%s\n' "$name" "$status" | tee -a "$ev/exits.tsv"
  test "$status" -eq 0
  test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
}
run profiler bash /home/meszmate/.cache/a165/scripts/build-prof.sh
run dist cargo build --profile dist -p pg-engine --bin roll-engine
mkdir -p /home/meszmate/.cache/a165/bin/new
cp "$CARGO_TARGET_DIR/dist/roll-engine" /home/meszmate/.cache/a165/bin/new/roll-engine
run clippy cargo clippy -p pg-engine --release
