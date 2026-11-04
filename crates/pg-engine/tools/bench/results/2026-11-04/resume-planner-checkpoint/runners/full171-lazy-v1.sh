#!/bin/bash
set -euo pipefail
export PATH=/home/meszmate/.cargo/bin:$PATH
export TMPDIR=/home/meszmate/.cache/a171/tmp
export CARGO_TARGET_DIR=/home/meszmate/.cache/a171/target
D=/home/meszmate/.cache/pg-engine-resume-20261104/session_planner
W=/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536
E="$D/full171-lazy-v1"
S=cac0933bc4eb5ef87674889542cab271de4615d0
cd "$W"
test -d "$TMPDIR" && test -d "$CARGO_TARGET_DIR"
mkdir -p "$E"
test ! -e "$E/source.txt"
# These two are explicitly cleared by the authorized gate. All other stress overrides stop.
unset GOLDEN_FILTER GOLDEN_RECORD
python3 - "$D" "$E" <<'PY'
import json,os,sys,subprocess,re
from pathlib import Path
D,E=map(Path,sys.argv[1:]);W=Path.cwd()
literals=set()
for p in(W/'crates/pg-engine').rglob('*.rs'):
 if 'target'in p.parts:continue
 literals.update(re.findall(r'env::var(?:_os)?\s*\(\s*"([A-Z0-9_]+)"',p.read_text()))
prefixes=('CRASH_','STRESS_','MEMLOAD_','DEEP_','GOLDEN_','LOCALE_','DIFFTEST_','PGREGRESS_','PGDUMP_','PGISOLATION_','PGCONTRIB_','PGVECTOR_','APPCONC_','WIRE_','RESTART_','PK_PARITY_','DEPARSE_','ROLL_E2E','RUST_TEST_','CARGO_PROFILE_')
keys=sorted(k for k in os.environ if k in literals or k.startswith(prefixes));allowed={'TMPDIR','RUST_TEST_THREADS','CARGO_PROFILE_DEV_DEBUG'}
blocked=[k for k in keys if k not in allowed]
r=dict(pid=os.getpid(),affinity=sorted(os.sched_getaffinity(0)),nice=os.getpriority(os.PRIO_PROCESS,0),jobs=os.environ.get('CARGO_BUILD_JOBS'),threads=os.environ.get('RUST_TEST_THREADS'),global_slots=Path('/tmp/pg-engine-heavy.lock.slots').read_text().strip(),global_jobs=Path('/tmp/pg-engine-heavy.lock.jobs').read_text().strip(),test_config={k:os.environ[k]for k in keys if k in allowed},nondefault_override_keys=blocked,golden_filter_unset='GOLDEN_FILTER'not in os.environ,golden_record_unset='GOLDEN_RECORD'not in os.environ,crash_default='no CRASH_* inherited;normal source defaults only')
(E/'startup-controls.json').write_text(json.dumps(r,indent=2)+'\n')
assert not blocked,blocked
assert r['affinity']==[8,9,10,11,12,13,18,19] and r['nice']==19 and r['jobs']=='2' and r['threads']=='2'and r['global_slots']=='4'and r['global_jobs']=='2',r
assert os.environ.get('CARGO_PROFILE_DEV_DEBUG')=='line-tables-only',r
PY
test "$(git -C "$W" rev-parse HEAD)" = "$S"
test "$(git -C "$W" rev-parse 'HEAD^{tree}')" = bd48e5074887125072d7c23cb09c78fe6fe15c0b
git -C "$W" rev-parse HEAD > "$E/source.txt"
git -C "$W" rev-parse 'HEAD^{tree}' > "$E/tree.txt"
git -C "$W" status --porcelain > "$E/initial.status"
test ! -s "$E/initial.status"
rustc -Vv > "$E/toolchain.txt"
cargo -V >> "$E/toolchain.txt"
check_source() {
 test "$(git -C "$W" rev-parse HEAD)" = "$S"
 git -C "$W" status --porcelain > "$E/current.status"
 cmp "$E/initial.status" "$E/current.status"
 sha256sum -c "$D/full171-frozen.sha256"
}
disk_floor() {
 python3 - "$E/disk-before-$1.json" <<'PY'
from pathlib import Path
import json,subprocess,sys,datetime
kib=int(subprocess.check_output(['df','-Pk','.']).decode().splitlines()[1].split()[3]);r=dict(observed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),available_kib=kib,floor_kib=30*1024*1024,pass_floor=kib>=30*1024*1024)
Path(sys.argv[1]).write_text(json.dumps(r,indent=2)+'\n');assert r['pass_floor'],r
PY
}
run() {
 local name=$1 status=0 audit=0; shift
 check_source
 disk_floor "$name"
 printf '%q ' "$@" > "$E/$name.command";printf '\n' >> "$E/$name.command"
 "$@" > "$E/$name.log" 2>&1 || status=$?
 printf '%s\t%s\n' "$name" "$status" | tee -a "$E/exits.tsv"
 test "$status" -eq 0 || return "$status"
 python3 "$D/full171-stage-audit.py" "$name" > "$E/$name-audit.log" 2>&1 || audit=$?
 printf '%s-audit\t%s\n' "$name" "$audit" | tee -a "$E/audit-exits.tsv"
 test "$audit" -eq 0 || return "$audit"
 check_source
}
run debug-test cargo test -p pg-engine
run release-test cargo test --release -p pg-engine
run clippy cargo clippy --release -p pg-engine --all-targets --message-format=json
run build cargo build --profile dist -p pg-engine --bin roll-engine
mkdir -p /home/meszmate/.cache/a171/bin/lazy-cac0933b
cp --reflink=auto "$CARGO_TARGET_DIR/dist/roll-engine" /home/meszmate/.cache/a171/bin/lazy-cac0933b/roll-engine
python3 - "$E/build-artifact.json" <<'PY'
import json,hashlib,subprocess,sys,os
from pathlib import Path
p=Path('/home/meszmate/.cache/a171/bin/lazy-cac0933b/roll-engine');original=Path(os.environ['CARGO_TARGET_DIR'])/'dist/roll-engine';h=hashlib.sha256(p.read_bytes()).hexdigest();assert h==hashlib.sha256(original.read_bytes()).hexdigest()
r=dict(binary=str(p),sha256=h,bytes=p.stat().st_size,profile='dist',source=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/full171-lazy-v1/source.txt').read_text().strip(),source_artifact=str(original),source_artifact_matches=True)
Path(sys.argv[1]).write_text(json.dumps(r,indent=2)+'\n')
PY
check_source
run postgres-unit pnpm test:pg --unit
echo FULL171-LAZY-FIVE-COMMANDS-AND-STAGE-AUDITS-PASS
