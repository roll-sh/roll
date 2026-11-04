#!/usr/bin/env bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a168/tmp CARGO_TARGET_DIR=/home/meszmate/.cache/a168/target
cd /home/meszmate/roll/.claude/worktrees/agent-ae7cd3533d1c88d16
out=/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/a168-oracle-checks
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
run build-release cargo test --release -p pg-engine --test crash_random --no-run
binary=$CARGO_TARGET_DIR/release/deps/crash_random-9e1c55c41accce42
sha256sum "$binary" > "$out/binary.sha256"
run negative-controls "$binary" --exact recovery_oracle_rejects_partial_and_lost_acknowledged_transactions --nocapture
run original-failure env CRASH_INSPECT=/home/meszmate/.cache/a168/saved/s3-r3 CRASH_SEED=3 "$binary" --exact crash_inspect --nocapture
run log-regression "$binary" --exact torn_acknowledgement_is_still_in_flight --nocapture
run controls-debug cargo test -p pg-engine --test crash_random recovery_oracle_rejects_partial_and_lost_acknowledged_transactions -- --exact --nocapture
run clippy cargo clippy -p pg-engine --test crash_random --test crash_async_commit
