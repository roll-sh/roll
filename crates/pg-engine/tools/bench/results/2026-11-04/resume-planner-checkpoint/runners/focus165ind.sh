#!/bin/bash
set -euo pipefail
source /home/meszmate/.cache/a165/env.sh
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/focus165ind
mkdir -p "$ev"
test ! -e "$ev/exits.tsv"
git -C "$W" rev-parse HEAD > "$ev/source.txt"
git -C "$W" rev-parse 'HEAD^{tree}' > "$ev/tree.txt"
git -C "$W" status --porcelain > "$ev/initial.status"
test ! -s "$ev/initial.status"
run() {
 local n=$1 status=0; shift
 printf '%q ' "$@" > "$ev/$n.command"; printf '\n' >> "$ev/$n.command"
 "$@" > "$ev/$n.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$n" "$status" | tee -a "$ev/exits.tsv"
 if [[ "$status" -ne 0 ]]; then return "$status"; fi
 grep -q 'test result: ok' "$ev/$n.log" || return 1
 if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$n.log"; then return 1; fi
 test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
 git -C "$W" status --porcelain > "$ev/current.status"
 cmp "$ev/initial.status" "$ev/current.status"
}
run lock cargo test -p pg-engine --test twophase --test compact_locks -- --nocapture
run kept cargo test -p pg-engine --test kept_plans -- --nocapture
run searchpath env GOLDEN_FILTER=search_path cargo test -p pg-engine --test golden -- --nocapture
run privileges env GOLDEN_FILTER=privileges cargo test -p pg-engine --test golden -- --nocapture
run wire cargo test -p pg-engine --test wire -- --nocapture
echo FOCUS165INDEPENDENT-PASS
