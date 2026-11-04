#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/probe165
mkdir -p "$ev"
run() {
 local n=$1 status=0; shift
 printf '%q ' "$@" > "$ev/$n.command"; printf '\n' >> "$ev/$n.command"
 "$@" > "$ev/$n.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$n" "$status" | tee -a "$ev/exits.tsv"
 return "$status"
}
run record env GOLDEN_RECORD=127.0.0.1:53310 GOLDEN_PASSWORD=pw GOLDEN_FILTER=a165_cursor_replay cargo test -p pg-engine --test golden -- --nocapture
if grep -q '^ERROR' "$E/tests/sql/a165_cursor_replay.out"; then exit 1; fi
run cursor env GOLDEN_FILTER=a165_cursor_replay cargo test -p pg-engine --test golden -- --nocapture
