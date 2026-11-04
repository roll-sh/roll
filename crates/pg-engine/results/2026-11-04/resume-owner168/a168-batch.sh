#!/usr/bin/env bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a168/tmp CARGO_TARGET_DIR=/home/meszmate/.cache/a168/target
cd /home/meszmate/roll/.claude/worktrees/agent-ae7cd3533d1c88d16
out=/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/a168-checks
mkdir -p "$out"
git rev-parse HEAD > "$out/source-head"
git status --porcelain=v1 > "$out/source-status"
git diff --binary HEAD > "$out/source-diff"
run() {
 local name=$1; shift
 printf 'START %s %s\n' "$name" "$(date -u +%FT%TZ)"
 local rc=0
 "$@" > "$out/$name.log" 2>&1 || rc=$?
 printf '%s EXIT %s\n' "$name" "$rc" | tee -a "$out/exits"
 tail -n 7 "$out/$name.log"
 test "$rc" -eq 0
}
run crash-build cargo test --release -p pg-engine --test crash_random --no-run
binary=$CARGO_TARGET_DIR/release/deps/crash_random-9e1c55c41accce42
test -x "$binary"
sha256sum "$binary" > "$out/crash-binary.sha256"
run ack-regression "$binary" --exact torn_acknowledgement_is_still_in_flight --nocapture
for round in $(seq 1 10); do
 run sync-$round env CRASH_SEEDS=1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30 CRASH_ROUNDS=4 "$binary" --exact random_crashes --nocapture
 grep -q '^test result: ok. 1 passed' "$out/sync-$round.log"
done
run async env CRASH_ASYNC=1 CRASH_SEEDS=1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30 CRASH_ROUNDS=4 "$binary" --exact random_crashes --nocapture
run targeted-debug cargo test -p pg-engine --test crash_random --test crash_ctids --test crash_unlogged --test crash_async_commit --test models_kept
run targeted-release cargo test --release -p pg-engine --test crash_ctids --test crash_unlogged --test crash_async_commit --test models_kept
run clippy cargo clippy -p pg-engine --all-targets
run pg-e2e pnpm test:pg
