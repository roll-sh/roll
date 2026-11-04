#!/bin/bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a171/tmp
export CARGO_TARGET_DIR=/home/meszmate/.cache/a171/target
W=/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536
D=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner
E="$D/parallel171-v2/runtime"
cd "$W"
mkdir -p "$E"
test ! -e "$E/source-start.txt"
test "$(git -C "$W" rev-parse HEAD)" = a6fe3e4e626b0552c14320c18211886d1b16ad38
git -C "$W" diff --binary > "$E/original.patch"
cmp "$E/original.patch" "$D/parallel171-v2/fixtures-v2.patch"
git -C "$W" diff --binary HEAD c8618834caf45bc0a34cf9985c07f9bc13b498ef -- crates/pg-engine/src > "$E/parent-transition.patch"
git -C "$W" rev-parse HEAD > "$E/source-start.txt"
sha256sum "$E/parent-transition.patch" "$E/original.patch" "$D/parallel171-v2.sh" > "$E/frozen.sha256"
transitioned=0
restore_source() {
 if [ "$transitioned" -eq 1 ]; then
   git -C "$W" apply -R "$E/parent-transition.patch" || return 120
   transitioned=0
 fi
 git -C "$W" diff --binary > "$E/restored.patch"
 cmp "$E/original.patch" "$E/restored.patch" || return 121
 git -C "$W" diff -- crates/pg-engine/src > "$E/restored-production.patch"
 test ! -s "$E/restored-production.patch" || return 122
}
finish() {
 local status=$?
 trap - EXIT
 restore_source || exit $?
 printf 'a6fe plus exact revised fixture patch restored; terminal runner status=%s\n' "$status" > "$E/RESTORED.txt"
 exit "$status"
}
trap finish EXIT
python3 - "$E/startup-controls.json" <<'PY'
import os,json,sys
from pathlib import Path
r=dict(pid=os.getpid(),affinity=sorted(os.sched_getaffinity(0)),nice=os.getpriority(os.PRIO_PROCESS,0),jobs=os.environ.get('CARGO_BUILD_JOBS'),threads=os.environ.get('RUST_TEST_THREADS'))
assert r['affinity']==[8,9,10,11,12,13,18,19] and r['nice']==19 and r['jobs']=='2' and r['threads']=='2',r
Path(sys.argv[1]).write_text(json.dumps(r,indent=2)+'\n')
PY
identity() {
 python3 - "$E" "$1" "$2" <<'PY'
import hashlib,json,os,subprocess,sys,tempfile
from pathlib import Path
E=Path(sys.argv[1]);phase=sys.argv[2];base=sys.argv[3];W='/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536'
def git(*a,env=None):return subprocess.check_output(['git','-C',W,*a],env=env)
assert git('diff',base,'--','crates/pg-engine/src')==b''
assert git('diff','--binary',base)==(E/'original.patch').read_bytes()
with tempfile.TemporaryDirectory(dir=os.environ['TMPDIR'])as t:
 env=dict(os.environ,GIT_INDEX_FILE=t+'/index');git('read-tree','HEAD',env=env);git('add','-u',env=env);tree=git('write-tree',env=env).decode().strip()
(E/(phase+'-identity.json')).write_text(json.dumps(dict(production=base,effective_tree=tree,head=git('rev-parse','HEAD').decode().strip(),fixturepatch_sha256=hashlib.sha256(git('diff','--binary',base)).hexdigest(),production_byte_identical=True),indent=2)+'\n')
(E/(phase+'-source.patch')).write_bytes(git('diff','--binary'))
PY
}
run() {
 local name=$1 expected=$2 status=0
 printf '%s\n' 'cargo test -p pg-engine --test parallel_scan parallel_workers_use_execution_context_after_parse -- --exact --nocapture' > "$E/$name.command"
 cargo test -p pg-engine --test parallel_scan parallel_workers_use_execution_context_after_parse -- --exact --nocapture > "$E/$name.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$name" "$status" | tee -a "$E/exits.tsv"
 python3 - "$E/$name.log" "$E/$name-binary.json" <<'PY'
import json,re,hashlib,sys,os
from pathlib import Path
s=Path(sys.argv[1]).read_text();m=re.search(r'Running .* \(([^\n]+)\)',s)
if not m:raise SystemExit('NO TEST BINARY: preserve compile/zero-tests failure')
p=Path(m[1]);data=dict(binary=str(p),sha256=hashlib.sha256(p.read_bytes()).hexdigest(),crate_compiling='Compiling pg-engine' in s,mtime_ns=p.stat().st_mtime_ns)
Path(sys.argv[2]).write_text(json.dumps(data,indent=2)+'\n')
assert data['crate_compiling'],'Expected own crate rebuild after scoped source transition; STOP, no rerun'
PY
 test "$status" -eq "$expected" || return 123
 if [ "$expected" -eq 0 ]; then
  grep -q 'test result: ok. 1 passed; 0 failed; 0 ignored;' "$E/$name.log"
  test "$(grep -Fc 'worker to_launch/launched delta=(2, 2)' "$E/$name.log")" -eq 2
  if grep -Eq '^error:|test result: FAILED|panicked at' "$E/$name.log"; then return 124; fi
 else
  grep -q 'test parallel_workers_use_execution_context_after_parse .*FAILED' "$E/$name.log"
  grep -q 'test result: FAILED. 0 passed; 1 failed;' "$E/$name.log"
  test "$(grep -Fc 'worker to_launch/launched delta=(0, 0)' "$E/$name.log")" -eq 2
  if grep -q '^error: could not compile' "$E/$name.log"; then return 125; fi
 fi
 sha256sum -c "$E/frozen.sha256"
}
git -C "$W" apply --check "$E/parent-transition.patch"
git -C "$W" apply "$E/parent-transition.patch"
transitioned=1
identity baseline c8618834caf45bc0a34cf9985c07f9bc13b498ef
run baseline 0
restore_source
printf 'a6fe+revisedfixtures byte-exact restored before candidate command\n' > "$E/RESTORED-BEFORE-CANDIDATE.txt"
identity candidate a6fe3e4e626b0552c14320c18211886d1b16ad38
run candidate 101
restore_source
echo PARALLEL171-V2-BOUNDED-PAIR-CONFIRMED-NO-SOURCE-ACCEPTANCE
