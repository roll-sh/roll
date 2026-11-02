"""Tie captured command exits to complete logs and the source versions they covered."""
from pathlib import Path
import hashlib,json
ROOT=Path(__file__).resolve().parent
REPO=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
def check(x,m):
    if not x:raise RuntimeError(m)
def sha(p):return hashlib.file_digest(Path(p).open('rb'),'sha256').hexdigest()
exits=json.loads((ROOT/'command-exits.json').read_text())
names={'full-crate':'tests-r1.log','clippy-all-targets':'clippy-r1.log','release-build':'release-build-r1.log','current-wire':'current-wire-r1.log'}
check(exits.keys()==names.keys(),'missing actual command exits')
source={p:sha(REPO/p) for p in ['crates/pg-engine/src/exec/sizing.rs','crates/pg-engine/tests/wire.rs']}
wire=(REPO/'crates/pg-engine/tests/wire.rs').read_text()
start=wire.index('        b.batch_execute(\n',wire.index('fn index_fetches_follow_visibility_changes'))
end=wire.index('        assert_eq!(lines(&b, query)',start)
old_wire=wire[:start]+'        b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").await.unwrap();\n'+wire[end:]
rows=[]
for name,log_name in names.items():
    check(exits[name]['exit_code']==0,name+' command failed')
    log=ROOT/log_name;text=log.read_text()
    if name in ['full-crate','current-wire']:
        check('index_fetches_follow_visibility_changes ... ok' in text,'missing visibility test')
        check('test result: FAILED' not in text,'test failures')
    if name=='current-wire':check('68 passed; 0 failed' in text,'incomplete current wire suite')
    if name=='release-build':check('Finished `release` profile' in text,'incomplete release build')
    if name=='clippy-all-targets':check('Finished `dev` profile' in text,'incomplete Clippy')
    hashes=dict(source)
    if name=='full-crate':hashes['crates/pg-engine/tests/wire.rs']=hashlib.sha256(old_wire.encode()).hexdigest()
    rows.append({'name':name,'exit':exits[name]['exit_code'],'execution_evidence':exits[name],'log':log_name,'log_sha256':sha(log),'source_sha256':hashes,'coverage_note':'Full-crate compiled earlier wire test; current-wire rerun covers second-session plan assertions.' if name=='full-crate' else 'Current source hashes; release production only depends on sizing.rs.'})
with (ROOT/'pretrial-checks.json').open('x') as f:json.dump(rows,f,indent=2);f.write('\n')
print('Recorded four zero exits, log hashes and source coverage.')
