#!/usr/bin/env bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a169/tmp CARGO_TARGET_DIR=/home/meszmate/.cache/a169/target
cd /home/meszmate/roll/.claude/worktrees/agent-a76932e8127f6d35b
out=/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/a169-reviewed
mkdir -p "$out"
git rev-parse HEAD > "$out/source-head"
git status --porcelain=v1 > "$out/source-status"
git diff --binary HEAD > "$out/source-diff"
find crates/pg-engine/src crates/pg-engine/tests crates/pg-engine/examples -type f -print0 | sort -z | xargs -0 sha256sum > "$out/source-sha256"
run() {
 local name=$1; shift
 local rc=0
 echo "START $name $(date -u +%FT%TZ)"
 "$@" > "$out/$name.log" 2>&1 || rc=$?
 echo "$name EXIT $rc" | tee -a "$out/exits"
 tail -n 10 "$out/$name.log"
 test "$rc" -eq 0
}
audit() { run "$1-summary" python3 /home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/check-rust-summary.py "$out/$1.log"; }
run proof-debug cargo test -p pg-engine --test storage_linked_gc --test storage_read_once
audit proof-debug
run storage-debug cargo test -p pg-engine --lib storage::
audit storage-debug
run integrations-debug cargo test -p pg-engine --test txn_spill --test drop_storage --test toast_storage --test big_tables --test storage_upgrade_batches
audit integrations-debug
run proof-release cargo test --release -p pg-engine --test storage_linked_gc --test storage_read_once --test txn_spill
audit proof-release
run crash-release env CRASH_SEEDS=1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30 CRASH_ROUNDS=4 cargo test --release -p pg-engine --test crash_random
audit crash-release
run storage-release cargo test --release -p pg-engine --lib storage::
audit storage-release
run clippy cargo clippy -p pg-engine --all-targets
run build cargo build --release -p pg-engine --bin roll-engine
sha256sum --check --quiet "$out/source-sha256"
echo SOURCE-UNCHANGED
echo 'pg-e2e DEFERRED: parent verification owns one final combined run; prior168/166 failures remain FAILED'
