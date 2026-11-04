#!/usr/bin/env bash
set -euo pipefail
cd /home/meszmate/roll/.claude/worktrees/agent-ac9369af470e262d2
out=/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/catalog166_review
binary=/home/meszmate/.cache/a166/target-debug/debug/deps/golden-9749ad749af5ca1c
sha256sum "$binary" > "$out/golden-binary.sha256"
run() {
 local name=$1; shift
 local rc=0
 "$@" > "$out/$name.log" 2>&1 || rc=$?
 echo "$name EXIT $rc" | tee -a "$out/golden-exits"
 tail -n 12 "$out/$name.log"
 test "$rc" -eq 0
}
run golden-record env GOLDEN_RECORD=127.0.0.1:53320 GOLDEN_PASSWORD=pw GOLDEN_FILTER=a166_lazy_snapshot "$binary" --exact golden --nocapture
run golden-before-fix env GOLDEN_FILTER=a166_lazy_snapshot "$binary" --exact golden --nocapture
