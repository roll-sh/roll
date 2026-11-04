#!/bin/bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a171/tmp
export CARGO_TARGET_DIR=/home/meszmate/.cache/a171/target
W=/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536
test -d "$CARGO_TARGET_DIR" && test -d "$TMPDIR"
cd "$W"
ev=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/check171-chosen-v1
mkdir -p "$ev"
python3 - "$ev" <<'CONTROL'
import json,os,sys
from pathlib import Path
r=dict(pid=os.getpid(),affinity=sorted(os.sched_getaffinity(0)),nice=os.getpriority(os.PRIO_PROCESS,0),cargo_jobs=os.environ.get('CARGO_BUILD_JOBS'),test_threads=os.environ.get('RUST_TEST_THREADS'))
assert r['affinity']==[8,9,10,11,12,13,18,19] and r['nice']==19 and r['cargo_jobs']=='2' and int(r['test_threads'])<=2,r
Path(sys.argv[1]+'/startup-controls.json').write_text(json.dumps(r,indent=2)+'\n')
print('STARTUP-CONTROLS-PASS',json.dumps(r))
CONTROL
test "$(git -C "$W" rev-parse HEAD)" = a6fe3e4e626b0552c14320c18211886d1b16ad38
test ! -e "$ev/exits.tsv"
git -C "$W" rev-parse HEAD > "$ev/source.txt"
git -C "$W" rev-parse 'HEAD^{tree}' > "$ev/tree.txt"
git -C "$W" status --porcelain > "$ev/initial.status"
test ! -s "$ev/initial.status"
rustc -Vv > "$ev/toolchain.txt"
cargo -V >> "$ev/toolchain.txt"
run() {
 local n=$1 status=0; shift
 printf '%q ' "$@" > "$ev/$n.command"; printf '\n' >> "$ev/$n.command"
 "$@" > "$ev/$n.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$n" "$status" | tee -a "$ev/exits.tsv"
 if [[ "$status" -ne 0 ]]; then return "$status"; fi
 if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$n.log"; then return 1; fi
 if [[ "$n" == debug-test || "$n" == release-test ]]; then
  python3 /home/meszmate/.cache/pg-engine-resume-20261104/session_planner/audit165-summary.py "$ev/$n.log" /home/meszmate/.cache/pg-engine-resume-20261104/session_planner/chosen171-expected-per-binary.json "$ev/$n-summary.json"
 fi
 if [[ "$n" == clippy || "$n" == build ]]; then
  grep -q 'Finished .*profile' "$ev/$n.log"
  if grep -Eq '^error|could not compile' "$ev/$n.log"; then return 1; fi
 fi
 if [[ "$n" == postgres-unit ]]; then
  grep -Eq '^postgres: (all green|no changes since the last green run; skipping)' "$ev/$n.log"
  if grep -Eq '^error|test result: FAILED|panicked at' "$ev/$n.log"; then return 1; fi
 fi
 test "$(git -C "$W" rev-parse HEAD)" = "$(cat "$ev/source.txt")"
 git -C "$W" status --porcelain > "$ev/current.status"
 cmp "$ev/initial.status" "$ev/current.status"
}
run debug-test env -u GOLDEN_FILTER -u GOLDEN_RECORD cargo test -p pg-engine
run release-test env -u GOLDEN_FILTER -u GOLDEN_RECORD cargo test --release -p pg-engine
run clippy cargo clippy --release -p pg-engine --all-targets
run build cargo build --profile dist -p pg-engine --bin roll-engine
mkdir -p /home/meszmate/.cache/a171/bin/chosen-cost
cp "$CARGO_TARGET_DIR/dist/roll-engine" /home/meszmate/.cache/a171/bin/chosen-cost/roll-engine
sha256sum /home/meszmate/.cache/a171/bin/chosen-cost/roll-engine > "$ev/engine.sha256"
run postgres-unit pnpm test:pg --unit
python3 - "$ev/postgres-unit.log" "$ev/postgres-unit-status.json" <<'UNIT'
import json,re,sys
from pathlib import Path
s=Path(sys.argv[1]).read_text()
skip='postgres: no changes since the last green run; skipping' in s
assert skip or 'postgres: all green' in s
r=dict(status='SKIP' if skip else 'PASS',executed_unit_checks=not skip,service_stack_acceptance=False,pg_engine_execution=False,reason='stamp matched; no checks executed' if skip else '--unit explicitly disables Docker e2e; separate final forced service-stack gate held')
Path(sys.argv[2]).write_text(json.dumps(r,indent=2)+'\n');print(json.dumps(r))
UNIT
echo CHECK171CHOSEN-PASS
