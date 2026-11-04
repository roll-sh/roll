#!/usr/bin/env bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export CARGO_TARGET_DIR=/home/meszmate/.cache/a163/target TMPDIR=/home/meszmate/.cache/a163/tmp
cd /home/meszmate/roll/.claude/worktrees/agent-a3512214927146502
out=/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/a163-checks
mkdir -p "$out"
git rev-parse HEAD > "$out/source-head"
git status --porcelain=v1 > "$out/source-status"
git diff --binary HEAD > "$out/source-diff"
run() {
 local name=$1; shift
 local rc=0
 echo "START $name $(date -u +%FT%TZ)"
 "$@" > "$out/$name.log" 2>&1 || rc=$?
 echo "$name EXIT $rc" | tee -a "$out/exits"
 tail -n 8 "$out/$name.log"
 test "$rc" -eq 0
}
run build cargo build -p pg-engine --bin roll-engine
run focused cargo test -p pg-engine --lib --test golden --test big_tables --test deep_input
grep -q '^test result: ok.' "$out/focused.log"
run clippy cargo clippy -p pg-engine --all-targets
run pgdump env PGDUMP_DIR=/home/meszmate/.cache/a163/v/resume-pgdump PGDUMP_CLIENT=a163-vcli PGDUMP_PORT=53267 PGDUMP_SRC=/home/meszmate/.cache/a163/v/rg/src-repo PGDUMP_REPORT_DIR=$out/pgdump-report cargo test -p pg-engine --test pg_dump -- --ignored --nocapture --exact pg_dump_tap
run pgdump-outcomes python3 /home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/check-tap.py /home/meszmate/.cache/a163/v/resume-pgdump
echo "pg-e2e DEFERRED: parent verification group owns one final combined run; prior a168/a166 failures remain FAILED"
