"""Post-measurement live audit and deletion of exactly the eight owned fixture copies."""
from pathlib import Path
import hashlib,importlib.util,json,shutil,socket,subprocess,sys,time
sys.dont_write_bytecode=True
ROOT=Path(__file__).resolve().parent
REPO=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
def check(x,m):
    if not x:raise RuntimeError(m)
def read(p):return json.loads((ROOT/p).read_text())
def sha(p):return hashlib.file_digest(Path(p).open('rb'),'sha256').hexdigest()
frame=read('frame.json')
for path,digest in frame['files_sha256'].items():check(sha(path)==digest,'frozen file changed: '+path)
spec=importlib.util.spec_from_file_location('diag','/home/meszmate/.cache/pg-engine-perf-20261102/planning-diagnostic/diagnose.py');d=importlib.util.module_from_spec(spec);spec.loader.exec_module(d)
phases=['monitor-correctness','fetch-primary-p1','control-dirty','fetch-confirmation-p1']
pids=[];helpers=[];removed=[];manifests=[]
for phase in phases:
    root=ROOT/phase;cleanup=read(phase+'/cleanup.json')
    check(len(cleanup)==3,'incomplete cleanup')
    for row in cleanup:
        if 'pid' in row:
            check(row['exit']==0 and row['stopped'],'engine exit failed')
            check(not Path('/proc',str(row['pid'])).exists(),'owned pid still exists')
            pids.append(row['pid'])
        else:
            check(row['remove_exit']==0,'helper cleanup failed')
            process=subprocess.run(['docker','inspect',row['helper_id']],capture_output=True,text=True)
            check(process.returncode!=0 and 'no such' in process.stderr.lower(),'helper still exists')
            helpers.append(row['helper_id'])
    manifests.append(read(phase+'/source-fixture-files.json'))
    for side in ['baseline','candidate']:
        data=root/(side+'-data')
        check(data.parent==root and not data.is_symlink(),'unsafe owned data path')
        if data.exists():shutil.rmtree(data);removed.append(str(data))
for port in range(53700,53708):
    with socket.socket() as sock:sock.bind(('127.0.0.1',port))
d.require_stopped(d.FIXTURE)
check(all(m==manifests[0] for m in manifests),'source fixture changed between phases')
check(d.tree_manifest(d.FIXTURE)==manifests[0],'original fixture changed')
result={'at_unix':time.time(),'owned_pids_absent':pids,'owned_helper_ids_absent':helpers,'ports_bind':list(range(53700,53708)),'removed_owned_paths':removed,'source_fixture_unchanged_across_all_phases':True,'frame_still_matches':True,'scope':'Live observation after timing; does not cover unrelated containers or later source-gate resources.'}
with (ROOT/'final-live-cleanup.json').open('x') as f:json.dump(result,f,indent=2);f.write('\n')
print('Eight measurement engines and four helpers absent; eight private copies removed; original fixture unchanged.')
