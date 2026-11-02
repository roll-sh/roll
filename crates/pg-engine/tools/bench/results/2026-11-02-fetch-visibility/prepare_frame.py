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
status=json.loads((ROOT/'pretrial-checks.json').read_text())
check(all(row['exit']==0 for row in status),'pretrial checks failed')
for row in status:
    check(sha(ROOT/row['log'])==row['log_sha256'],'pretrial log changed')
    for path,digest in row['source_sha256'].items():
        if row['name']=='full-crate' and path.endswith('/tests/wire.rs'):continue
        check(sha(REPO/path)==digest,'pretrial source changed')
check('index_fetches_follow_visibility_changes ... ok' in (ROOT/'current-wire-r1.log').read_text(),'current visibility test missing')
check(json.loads((ROOT/'clippy-comparison.json').read_text())['passes'],'Clippy warning comparison failed')
check({r['name'] for r in status}=={'full-crate','clippy-all-targets','current-wire','release-build'},'missing pretrial checks')
manifest=json.loads((ROOT/'candidate-bin/manifest.json').read_text())
for p,h in manifest['source_sha256'].items():check(sha(REPO/p)==h,'source changed '+p)
check(len(manifest['source_sha256'])==2,'must freeze both changed files')
check(subprocess.check_output(['git','diff','--binary'],cwd=REPO)==(ROOT/'candidate-source/tracked.patch').read_bytes(),'tracked patch differs')
for p in ROOT.glob('*.py'):ast.parse(p.read_text())
for diagnostic, cleanup_name, control, swapped in [('monitor-diagnostic-r1','monitor-diagnostic-cleanup.json',None,False),('dirty-diagnostic-r1','dirty-diagnostic-cleanup.json','dirty',False),('swap-diagnostic-r1','swap-diagnostic-cleanup.json',None,True)]:
    n=ROOT/diagnostic
    check(json.loads((ROOT/(diagnostic+'-status.json')).read_text())['exit']==0,'diagnostic command failed')
    check(json.loads((n/'complete.json').read_text())['benchmark_executed'] is False,'diagnostic ran benchmark')
    check(json.loads((n/'host-observation.json').read_text())['errors']==[],'diagnostic observer errors')
    check(json.loads((n/'host-observation.json').read_text())['stopped'],'diagnostic observer live')
    check(len(json.loads((n/'diagnostic-fixture-digests.json').read_text()))==2,'missing diagnostic digests')
    runtime=json.loads((n/'runtime.json').read_text())
    check(runtime['control_phase']==control and runtime['slots_swapped']==swapped,'wrong diagnostic phase')
    check(runtime['engines']['candidate']['sha256']==manifest['binary_sha256'],'wrong diagnostic candidate')
    ports={name: value['port'] for name,value in runtime['engines'].items()}
    check((ports['candidate']<ports['baseline'])==swapped,'wrong diagnostic binary slot')
    for side in ['baseline','candidate']:
        for moment in ['before','after']:
            visibility=json.loads((n/(side+'-visibility-'+moment+'.json')).read_text())
            check(len(visibility)==1 and int(visibility[0][0])>0,'missing diagnostic visibility')
            check(int(visibility[0][1])==(0 if control=='dirty' else int(visibility[0][0])),'wrong diagnostic heap state')
    cleanup=json.loads((n/'cleanup.json').read_text())
    check(len(cleanup)==3,'diagnostic cleanup incomplete')
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
    save(ROOT/cleanup_name,{'at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'live_absent':True,'removed_owned_paths':removed})
for port in list(range(53700,53708))+[53720]:
    with socket.socket() as s:s.bind(('127.0.0.1',port))
paths=set()
for p in (REPO/'crates/pg-engine').rglob('*.rs'):
    if '/tools/bench/results/' not in str(p):paths.add(p)
for p in REPO.rglob('Cargo.toml'):
    if '/target/' not in str(p) and '/node_modules/' not in str(p):paths.add(p)
paths.add(REPO/'Cargo.lock')
paths.add(REPO/'package.json')
paths.add(REPO/'scripts/test-postgres.sh')
paths.update(p for p in (REPO/'scripts').glob('*test*pg*') if p.is_file())
for p in json.loads((OLD/'proof-frame.json').read_text())['files_sha256']:
    p=Path(p)
    if not p.is_relative_to(OLD) and p.exists():paths.add(p)
for p in [OLD/'baseline-bin/roll-engine',OLD/'baseline-bin/manifest.json',OLD/'baseline-bin/lib-pg_engine.json']:
    paths.add(p)
for name in ['HYPOTHESIS.md','PLAN.md','review-response.json','matched-dependencies.json','pretrial-review-r1.json','pretrial-review-r2.json','tests-r1.log','clippy-r1.log','current-wire-r1.log','pretrial-checks.json','command-exits.json','gate-validation.json','release-build-r1.log','format-qualification.json','monitor-diagnostic-cleanup.json']:
    paths.add(ROOT/name)
paths.update(ROOT.glob('*.py'))
paths.update(ROOT.glob('*-diagnostic-r1-status.json'))
paths.add(ROOT/'diagnostics-controller.log')
paths.update(p for p in (ROOT/'candidate-bin').rglob('*') if p.is_file())
paths.update(p for p in (ROOT/'candidate-source').rglob('*') if p.is_file())
paths.update(p for p in (ROOT/'monitor-diagnostic-r1').glob('*.json') if p.is_file())
paths.update(p for p in (ROOT/'dirty-diagnostic-r1').glob('*.json') if p.is_file())
paths.add(ROOT/'dirty-diagnostic-cleanup.json')
paths.update(p for p in (ROOT/'swap-diagnostic-r1').glob('*.json') if p.is_file())
paths.add(ROOT/'swap-diagnostic-cleanup.json')
paths.add(ROOT/'clippy-comparison.json')
paths.add(Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/final-gates/clippy.log'))
paths.add(Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/final-gates/commit.txt'))
paths.add(Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/final-gates/git-status.txt'))
paths.add(Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/final-gates/status.tsv'))
check(not any(ROOT.glob('fetch-primary-p1/candidate-vs-baseline/*.log')),'samples already exist')
frame={'at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=REPO,text=True).strip(),'candidate_sha256':manifest['binary_sha256'],'baseline_sha256':sha(OLD/'baseline-bin/roll-engine'),'python':sys.version,'rule':'One prospective launch, no relaunch including zero-sample failure; reject on any harness/evaluation or declared performance failure; all acceptance gates frozen before launch.','files_sha256':{str(p):sha(p) for p in sorted(paths)}}
save(ROOT/'frame.json',frame)
save(ROOT/'launch-health.json',{'at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'loadavg':Path('/proc/loadavg').read_text(),'disk_free_bytes':shutil.disk_usage(REPO).free,'cpus':os.cpu_count(),'frame_sha256':sha(ROOT/'frame.json'),'timed_samples_before_launch':0})
print(json.dumps({'frozen_inputs':len(paths),'frame_sha256':sha(ROOT/'frame.json'),'candidate':manifest['binary_sha256']}))
