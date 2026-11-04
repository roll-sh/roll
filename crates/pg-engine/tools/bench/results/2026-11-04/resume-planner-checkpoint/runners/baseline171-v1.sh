#!/bin/bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a171/tmp
export CARGO_TARGET_DIR=/home/meszmate/.cache/a171/target
W=/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536
D=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner
E="$D/baseline171-v1"
cd "$W"
mkdir -p "$E"
test ! -e "$E/source.txt"
test "$(git -C "$W" rev-parse HEAD)" = a6fe3e4e626b0552c14320c18211886d1b16ad38
git -C "$W" diff --binary > "$E/original.patch"
cmp "$E/original.patch" "$D/red171-v1/fixtures.patch"
git -C "$W" diff --binary HEAD c8618834caf45bc0a34cf9985c07f9bc13b498ef -- crates/pg-engine/src > "$E/parent-transition.patch"
sha256sum "$E/parent-transition.patch" "$E/original.patch" "$D/baseline171-v1.sh" > "$E/frozen.sha256"
transitioned=0
restore() {
 local status=$?
 trap - EXIT
 if [ "$transitioned" -eq 1 ]; then
   git -C "$W" apply -R "$E/parent-transition.patch" || exit 120
 fi
 git -C "$W" diff --binary > "$E/restored.patch"
 cmp "$E/original.patch" "$E/restored.patch" || exit 121
 git -C "$W" diff -- crates/pg-engine/src > "$E/restored-production.patch"
 test ! -s "$E/restored-production.patch" || exit 122
 printf 'a6fe plus exact original fixture patch restored; terminal runner status=%s\n' "$status" > "$E/RESTORED.txt"
 exit "$status"
}
trap restore EXIT
git -C "$W" apply --check "$E/parent-transition.patch"
git -C "$W" apply "$E/parent-transition.patch"
transitioned=1
python3 - "$E" <<'PY'
import hashlib,json,os,subprocess,sys,tempfile
from pathlib import Path
E=Path(sys.argv[1]);W='/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536';D=E.parent
r=dict(pid=os.getpid(),affinity=sorted(os.sched_getaffinity(0)),nice=os.getpriority(os.PRIO_PROCESS,0),jobs=os.environ.get('CARGO_BUILD_JOBS'),threads=os.environ.get('RUST_TEST_THREADS'))
assert r['affinity']==[8,9,10,11,12,13,18,19] and r['nice']==19 and r['jobs']=='2' and r['threads']=='2',r
(E/'startup-controls.json').write_text(json.dumps(r,indent=2)+'\n')
def git(*a,env=None):return subprocess.check_output(['git','-C',W,*a],env=env)
base='c8618834caf45bc0a34cf9985c07f9bc13b498ef'
assert git('diff',base,'--','crates/pg-engine/src')==b''
assert git('diff','--binary',base)==(D/'red171-v1/fixtures.patch').read_bytes()
with tempfile.TemporaryDirectory(dir=os.environ['TMPDIR'])as t:
 env=dict(os.environ,GIT_INDEX_FILE=t+'/index');git('read-tree','HEAD',env=env);git('add','-u',env=env);tree=git('write-tree',env=env).decode().strip()
(E/'source.txt').write_text(base+' parent production + unchanged exact fixture patch; HEAD intentionally remainsa6fe\n')
(E/'tree.txt').write_text(tree+'\n')
(E/'source.patch').write_bytes(git('diff','--binary'))
(E/'testpatch.sha256').write_text(hashlib.sha256(git('diff','--binary',base)).hexdigest()+'\n')
PY
run() {
 local name=$1 target=$2 filter=$3 status=0
 printf '%s\n' "cargo test -p pg-engine --test $target $filter -- --exact --nocapture" > "$E/$name.command"
 cargo test -p pg-engine --test "$target" "$filter" -- --exact --nocapture > "$E/$name.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$name" "$status" | tee -a "$E/exits.tsv"
 python3 - "$E/$name.log" "$E/$name-binary.json" <<'PY'
import json,re,hashlib,sys
from pathlib import Path
s=Path(sys.argv[1]).read_text();m=re.search(r'Running .* \(([^\n]+)\)',s)
if not m: raise SystemExit('NO TEST BINARY: preserve compile/zero-tests failure')
p=Path(m[1]);Path(sys.argv[2]).write_text(json.dumps(dict(binary=str(p),sha256=hashlib.sha256(p.read_bytes()).hexdigest(),crate_compiling='Compiling pg-engine' in s),indent=2)+'\n')
PY
 test "$status" -eq 0 || return "$status"
 grep -q "test $filter .*ok" "$E/$name.log"
 grep -q 'test result: ok. 1 passed; 0 failed; 0 ignored;' "$E/$name.log"
 if grep -Eq '^error:|test result: FAILED|panicked at' "$E/$name.log"; then return 125; fi
 git -C "$W" diff --binary c8618834caf45bc0a34cf9985c07f9bc13b498ef > "$E/current-fixtures.patch"
 cmp "$E/current-fixtures.patch" "$E/original.patch"
 sha256sum -c "$E/frozen.sha256"
}
run generic kept_plans generic_cost_uses_choice_context_after_parse
run parallel parallel_scan parallel_workers_use_execution_context_after_parse
echo BASELINE171-TWO-PASS
