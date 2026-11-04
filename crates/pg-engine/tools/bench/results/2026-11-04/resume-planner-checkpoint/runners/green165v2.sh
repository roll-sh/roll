#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/green165v2
mkdir -p "$ev"
test ! -e "$ev/exits.tsv"
git -C "$W" rev-parse HEAD > "$ev/head.txt"
git -C "$W" diff --binary > "$ev/source.patch"
sha256sum "$E/tests/sql/a165_cursor_"{stream,replay}.{sql,out} > "$ev/goldens.sha256"
run() {
 local n=$1 status=0; shift
 printf '%q ' "$@" > "$ev/$n.command"; printf '\n' >> "$ev/$n.command"
 "$@" > "$ev/$n.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$n" "$status" | tee -a "$ev/exits.tsv"
 test "$status" -eq 0
 grep -q 'test result: ok' "$ev/$n.log"
 if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$n.log"; then return 1; fi
 test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/head.txt")"
 git -C "$W" diff --binary > "$ev/current.patch"
 cmp "$ev/source.patch" "$ev/current.patch"
 sha256sum --check "$ev/goldens.sha256"
}
run lock cargo test -p pg-engine --test twophase -- --nocapture
run cursor env GOLDEN_FILTER=a165_cursor_ cargo test -p pg-engine --test golden -- --nocapture
run memory cargo test -p pg-engine --test memory_load streaming_cursor_bounds_keys_and_backward_work -- --nocapture
echo GREEN165V2-PASS
