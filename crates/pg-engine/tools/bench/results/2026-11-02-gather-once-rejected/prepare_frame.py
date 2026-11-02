"""Seal the prospective inputs after independently checked build and diagnostic cleanup."""
from pathlib import Path
import ast,datetime,hashlib,json,os,shutil,socket,subprocess,sys
ROOT=Path(__file__).resolve().parent
REPO=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
OLD=ROOT.parent/'gather-lifecycle'
def check(b,s):
    if not b:raise RuntimeError(s)
def sha(p):return hashlib.file_digest(Path(p).open('rb'),'sha256').hexdigest()
def save(p,x):
    with p.open('x') as f:json.dump(x,f,indent=2);f.write('\n')
check(not (ROOT/'frame.json').exists(),'frame already exists')
manifest=json.loads((ROOT/'candidate-bin/manifest.json').read_text())
for p,h in manifest['source_sha256'].items():check(sha(REPO/p)==h,'source changed '+p)
check(len(manifest['source_sha256'])==7,'must freeze seven changed files')
check(subprocess.check_output(['git','diff','--binary'],cwd=REPO)==(ROOT/'candidate-source/tracked.patch').read_bytes(),'tracked patch differs')
for p in ROOT.glob('*.py'):ast.parse(p.read_text())
n=ROOT/'monitor-diagnostic-r1'
check(json.loads((n/'complete.json').read_text())['benchmark_executed'] is False,'diagnostic ran benchmark')
check(json.loads((n/'host-observation.json').read_text())=={'errors':[],'stopped':True,'scope':'Descriptive host CPU and frequency observation; not exclusive CPU ownership or profiling; starts before server launch and warmup.'},'observer mismatch')
cleanup=json.loads((n/'cleanup.json').read_text())
for row in cleanup:
    if 'pid' in row:
        check(row['exit']==0 and row['stopped'],'engine cleanup failed')
        check(not Path('/proc',str(row['pid'])).exists(),'diagnostic pid still exists')
    else:
        check(row['remove_exit']==0,'helper cleanup failed')
        result=subprocess.run(['docker','inspect',row['helper_id']],text=True,capture_output=True)
        check(result.returncode!=0 and 'no such' in result.stderr.lower(),'helper still exists')
removed=[]
for name in ['baseline-data','candidate-data']:
    p=n/name
    check(p.parent==n and not p.is_symlink(),'unsafe data path')
    if p.exists():shutil.rmtree(p);removed.append(str(p))
save(ROOT/'monitor-diagnostic-cleanup.json',{'at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'live_absent':True,'removed_owned_paths':removed})
for port in list(range(53600,53610))+[53620]:
    with socket.socket() as s:s.bind(('127.0.0.1',port))
paths=set()
for p in (REPO/'crates/pg-engine').rglob('*.rs'):
    if '/tools/bench/results/' not in str(p):paths.add(p)
for p in REPO.rglob('Cargo.toml'):
    if '/target/' not in str(p) and '/node_modules/' not in str(p):paths.add(p)
paths.add(REPO/'Cargo.lock')
for p in json.loads((OLD/'proof-frame.json').read_text())['files_sha256']:
    p=Path(p)
    if not p.is_relative_to(OLD):paths.add(p)
for p in [OLD/'baseline-bin/roll-engine',OLD/'baseline-bin/manifest.json',OLD/'baseline-bin/lib-pg_engine.json']:
    paths.add(p)
for name in ['HYPOTHESIS.md','DESIGN.md','matched-dependencies.json','source-review-r1.json','pretrial-review-r2.json','library-r4.log','clippy-r1.log','release-build-r1.log','monitor-diagnostic-cleanup.json']:
    paths.add(ROOT/name)
paths.update(ROOT.glob('*.py'))
paths.update(p for p in (ROOT/'candidate-bin').rglob('*') if p.is_file())
paths.update(p for p in (ROOT/'candidate-source').rglob('*') if p.is_file())
paths.update(p for p in n.glob('*.json') if p.is_file())
check(not any(ROOT.glob('proof-primary-p1/candidate-vs-baseline/*.log')),'samples already exist')
frame={'at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=REPO,text=True).strip(),'candidate_sha256':manifest['binary_sha256'],'baseline_sha256':sha(OLD/'baseline-bin/roll-engine'),'python':sys.version,'rule':'One prospective launch, no relaunch including zero-sample failure; reject on any harness/evaluation or declared performance failure; all acceptance gates frozen before launch.','files_sha256':{str(p):sha(p) for p in sorted(paths)}}
save(ROOT/'frame.json',frame)
save(ROOT/'launch-health.json',{'at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'loadavg':Path('/proc/loadavg').read_text(),'disk_free_bytes':shutil.disk_usage(REPO).free,'cpus':os.cpu_count(),'frame_sha256':sha(ROOT/'frame.json'),'timed_samples_before_launch':0})
print(json.dumps({'frozen_inputs':len(paths),'frame_sha256':sha(ROOT/'frame.json'),'candidate':manifest['binary_sha256']}))
