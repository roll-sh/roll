#!/bin/bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a171/tmp
export CARGO_TARGET_DIR=/home/meszmate/.cache/a171/target
W=/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536
D=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner
E="$D/focused171-lazy-v1"
S=cac0933bc4eb5ef87674889542cab271de4615d0
cd "$W"
test -d "$TMPDIR" && test -d "$CARGO_TARGET_DIR"
mkdir -p "$E"
test ! -e "$E/source.txt"
test "$(git -C "$W" rev-parse HEAD)" = "$S"
git -C "$W" status --porcelain > "$E/initial.status"
test ! -s "$E/initial.status"
git -C "$W" rev-parse HEAD > "$E/source.txt"
git -C "$W" rev-parse 'HEAD^{tree}' > "$E/tree.txt"
git -C "$W" diff --binary c8618834caf45bc0a34cf9985c07f9bc13b498ef > "$E/source.patch"
sha256sum -c "$D/focused171-lazy-v1-frozen.sha256"
python3 - "$E/startup-controls.json" <<'PY'
import os,json,sys,subprocess
from pathlib import Path
r=dict(pid=os.getpid(),affinity=sorted(os.sched_getaffinity(0)),nice=os.getpriority(os.PRIO_PROCESS,0),jobs=os.environ.get('CARGO_BUILD_JOBS'),threads=os.environ.get('RUST_TEST_THREADS'),global_slots=Path('/tmp/pg-engine-heavy.lock.slots').read_text().strip(),global_jobs=Path('/tmp/pg-engine-heavy.lock.jobs').read_text().strip(),free_kib=int(subprocess.check_output(['df','-Pk','.']).decode().splitlines()[1].split()[3]),disk_floor_gib=30)
assert r['affinity']==[8,9,10,11,12,13,18,19] and r['nice']==19 and r['jobs']=='2' and r['threads']=='2',r
assert r['global_slots']=='4' and r['global_jobs']=='2' and r['free_kib']>=30*1024*1024,r
Path(sys.argv[1]).write_text(json.dumps(r,indent=2)+'\n')
PY
check_source() {
 test "$(git -C "$W" rev-parse HEAD)" = "$S"
 git -C "$W" status --porcelain > "$E/current.status"
 cmp "$E/initial.status" "$E/current.status"
 sha256sum -c "$D/focused171-lazy-v1-frozen.sha256"
}
run_test() {
 local name=$1 expected=$2 status=0; shift 2
 printf '%q ' "$@" > "$E/$name.command";printf '\n' >> "$E/$name.command"
 "$@" > "$E/$name.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$name" "$status" | tee -a "$E/exits.tsv"
 python3 - "$E/$name.log" "$E/$name-binary.json" <<'PY'
import json,re,hashlib,sys
from pathlib import Path
s=Path(sys.argv[1]).read_text();m=re.search(r'Running .* \(([^\n]+)\)',s)
if not m:raise SystemExit('NO TEST BINARY: compile/zero-tests distinction preserved')
p=Path(m[1]);Path(sys.argv[2]).write_text(json.dumps(dict(binary=str(p),sha256=hashlib.sha256(p.read_bytes()).hexdigest(),mtime_ns=p.stat().st_mtime_ns,crate_compiling='Compiling pg-engine' in s),indent=2)+'\n')
PY
 test "$status" -eq 0 || return "$status"
 grep -q "test result: ok. $expected passed; 0 failed; 0 ignored;" "$E/$name.log"
 if grep -Eq '^error:|test result: FAILED|panicked at' "$E/$name.log";then return 125;fi
 case "$name" in
 generic) grep -Fq 'six Binds after RESET: [Int8(1), Int8(5)]' "$E/$name.log" ;;
 parallel) test "$(grep -Fc 'worker to_launch/launched delta=(2, 2)' "$E/$name.log")" -eq 2 ;;
 cost-golden) grep -q '^test golden .*ok' "$E/$name.log";grep -q '^test golden_shuffled .*ok' "$E/$name.log";grep -q '^test golden_wire .*ok' "$E/$name.log";test "$(grep -c '^1 golden files match' "$E/$name.log")" -eq 3 ;;
 esac
 check_source
}
run_test generic 1 cargo test -p pg-engine --test kept_plans generic_cost_uses_choice_context_after_parse -- --exact --nocapture
run_test parallel 1 cargo test -p pg-engine --test parallel_scan parallel_workers_use_execution_context_after_parse -- --exact --nocapture
run_test logical-parallel 1 cargo test -p pg-engine --test logical_apply parallel_apply -- --exact --nocapture
run_test logical-prepared-stream 1 cargo test -p pg-engine --test logical_apply prepared_stream_without_two_phase_cleans_up -- --exact --nocapture
run_test kept 8 cargo test -p pg-engine --test kept_plans -- --nocapture
run_test cost-golden 3 env GOLDEN_FILTER=a145_oltp_plans cargo test -p pg-engine --test golden -- --nocapture
printf '%s\n' 'cargo clippy -p pg-engine --lib --message-format=json' > "$E/clippy.command"
status=0
cargo clippy -p pg-engine --lib --message-format=json > "$E/clippy.log" 2>&1 || status=$?
printf 'clippy\t%s\n' "$status" | tee -a "$E/exits.tsv"
test "$status" -eq 0
python3 - "$E/clippy.log" "$E/clippy-summary.json" <<'PY'
import json,sys
from pathlib import Path
rows=[]
for line in Path(sys.argv[1]).read_text().splitlines():
 try:rows.append(json.loads(line))
 except json.JSONDecodeError:pass
assert any(r.get('reason')=='build-finished' and r.get('success')for r in rows),'Missing successful Clippy build summary'
errors=[r for r in rows if r.get('reason')=='compiler-message' and r['message']['level']=='error'];assert not errors,errors
warnings=[dict(message=r['message']['message'],code=(r['message'].get('code')or{}).get('code'),paths=sorted({s['file_name']for s in r['message'].get('spans',[])if s.get('is_primary') }))for r in rows if r.get('reason')=='compiler-message' and r['message']['level']=='warning']
Path(sys.argv[2]).write_text(json.dumps(dict(command_exit=0,build_finished_success=True,errors=len(errors),warnings=warnings,scope='library Clippy only; no fresh parent warning-comparison run'),indent=2)+'\n')
PY
check_source
echo FOCUSED171-LAZY-ALL-SEVEN-COMMANDS-PASS
