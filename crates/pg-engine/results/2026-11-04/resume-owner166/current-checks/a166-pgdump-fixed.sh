#!/usr/bin/env bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export CARGO_TARGET_DIR=/home/meszmate/.cache/a166/target-debug TMPDIR=/home/meszmate/.cache/a166/tmp
cd /home/meszmate/roll/.claude/worktrees/agent-ac9369af470e262d2
out=/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/a166-pgdump-fixed
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
run pgdump env PGDUMP_DIR=/home/meszmate/.cache/a166/pgr1/resume-pgdump PGDUMP_CLIENT=a166-psql1 PGDUMP_PORT=53325 PGDUMP_SRC=/home/meszmate/.cache/a166/pgr1/src-repo PGDUMP_REPORT_DIR=$out/pgdump-fixed-report cargo test -p pg-engine --test pg_dump -- --ignored --nocapture --exact pg_dump_tap
run pgdump-outcomes python3 /home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/check-tap.py /home/meszmate/.cache/a166/pgr1/resume-pgdump
