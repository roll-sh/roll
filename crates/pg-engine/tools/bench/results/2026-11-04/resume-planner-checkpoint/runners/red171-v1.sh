#!/bin/bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a171/tmp
export CARGO_TARGET_DIR=/home/meszmate/.cache/a171/target
W=/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536
D=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner
E="$D/red171-v1"
cd "$W"
mkdir -p "$E"
test ! -e "$E/source.txt"
git -C "$W" rev-parse HEAD > "$E/source.txt"
test "$(cat "$E/source.txt")" = a6fe3e4e626b0552c14320c18211886d1b16ad38
git -C "$W" diff -- crates/pg-engine/src > "$E/production.patch"
test ! -s "$E/production.patch"
git -C "$W" diff --binary > "$E/fixtures.patch"
sha256sum "$E/fixtures.patch" "$D/red171-v1.sh" > "$E/frozen.sha256"
python3 - "$E/startup-controls.json" <<'PY'
import os,json,sys
from pathlib import Path
r=dict(pid=os.getpid(),affinity=sorted(os.sched_getaffinity(0)),nice=os.getpriority(os.PRIO_PROCESS,0),jobs=os.environ.get('CARGO_BUILD_JOBS'),threads=os.environ.get('RUST_TEST_THREADS'))
assert r['affinity']==[8,9,10,11,12,13,18,19] and r['nice']==19 and r['jobs']=='2' and int(r['threads'])<=2,r
Path(sys.argv[1]).write_text(json.dumps(r,indent=2)+'\n')
PY
red() {
 local name=$1 target=$2 filter=$3 status=0
 printf '%s\n' "cargo test -p pg-engine --test $target $filter -- --exact --nocapture" > "$E/$name.command"
 cargo test -p pg-engine --test "$target" "$filter" -- --exact --nocapture > "$E/$name.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$name" "$status" | tee -a "$E/exits.tsv"
 test "$status" -eq 101
 grep -q "test $filter .*FAILED" "$E/$name.log"
 grep -q 'test result: FAILED. 0 passed; 1 failed;' "$E/$name.log"
 ! grep -q '^error: could not compile' "$E/$name.log"
 git -C "$W" diff --binary > "$E/current.patch"
 cmp "$E/fixtures.patch" "$E/current.patch"
 sha256sum -c "$E/frozen.sha256"
}
red generic kept_plans generic_cost_uses_choice_context_after_parse
red parallel parallel_scan parallel_workers_use_execution_context_after_parse
echo RED171-EXPECTED-FAILURES-CONFIRMED
