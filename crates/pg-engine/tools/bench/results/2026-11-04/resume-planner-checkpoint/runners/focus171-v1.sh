#!/bin/bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a171/tmp
export CARGO_TARGET_DIR=/home/meszmate/.cache/a171/target
W=/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536
D=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner
ev="$D/focus171-v1"
cd "$W"
test -d "$CARGO_TARGET_DIR" && test -d "$TMPDIR"
mkdir -p "$ev"
test ! -e "$ev/source.txt"
test "$(git -C "$W" rev-parse HEAD)" = a6fe3e4e626b0552c14320c18211886d1b16ad38
git -C "$W" rev-parse HEAD > "$ev/source.txt"
git -C "$W" rev-parse 'HEAD^{tree}' > "$ev/tree.txt"
git -C "$W" status --porcelain > "$ev/initial.status"
test ! -s "$ev/initial.status"
python3 - "$ev/startup-controls.json" <<'PY'
import json,os,sys
from pathlib import Path
r=dict(pid=os.getpid(),affinity=sorted(os.sched_getaffinity(0)),nice=os.getpriority(os.PRIO_PROCESS,0),cargo_jobs=os.environ.get('CARGO_BUILD_JOBS'),test_threads=os.environ.get('RUST_TEST_THREADS'))
assert r['affinity']==[8,9,10,11,12,13,18,19] and r['nice']==19 and r['cargo_jobs']=='2' and int(r['test_threads'])<=2,r
Path(sys.argv[1]).write_text(json.dumps(r,indent=2)+'\n')
print('STARTUP-CONTROLS-PASS',json.dumps(r))
PY
run() {
 local n=$1 count=$2 status=0; shift 2
 printf '%q ' "$@" > "$ev/$n.command"; printf '\n' >> "$ev/$n.command"
 "$@" > "$ev/$n.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$n" "$status" | tee -a "$ev/exits.tsv"
 test "$status" -eq 0 || return "$status"
 grep -q "test result: ok. $count passed; 0 failed; 0 ignored;" "$ev/$n.log"
 if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$n.log"; then return 1; fi
 test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
 git -C "$W" status --porcelain > "$ev/current.status"
 cmp "$ev/initial.status" "$ev/current.status"
}
run cost-golden 3 env GOLDEN_FILTER=a145_oltp_plans cargo test -p pg-engine --test golden -- --nocapture
run kept 7 cargo test -p pg-engine --test kept_plans -- --nocapture
echo FOCUS171-PASS
