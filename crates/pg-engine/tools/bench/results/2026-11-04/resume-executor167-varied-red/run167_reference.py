#!/usr/bin/env python3
import hashlib,json,os,re,subprocess,sys
from pathlib import Path
r=Path('/home/meszmate/.cache/pg-engine-resume-20261104/execution_models')
wt=Path('/home/meszmate/roll/.claude/worktrees/agent-a6193f5e45a7ae4f1')
out=r/'a167-varied-red';out.mkdir(exist_ok=True)
def git(*args):return subprocess.check_output(['git','-C',str(wt),*args]).decode().strip()
binary=Path('/home/meszmate/.cache/a167/target/debug/deps/golden-9749ad749af5ca1c')
h=hashlib.sha256()
with binary.open('rb') as f:
 for chunk in iter(lambda:f.read(1<<20),b''):h.update(chunk)
assert h.hexdigest()=='e7a9bfce8af8a25d8563044a661cf49cfd093b71a7d71b3abfc2bf25309bfab8'
source={'head':git('rev-parse','HEAD'),'tree':git('rev-parse','HEAD^{tree}'),'status':git('status','--porcelain=v1'),
        'binary':str(binary),'binary_sha256':h.hexdigest(),'effective_cpus':sorted(os.sched_getaffinity(0)),
        'note':'Retained binary built and verified at clean c1594cc6; only new runtime SQL fixture added, production unchanged.'}
assert source['head']=='c1594cc6c9fac649ee76a86d711e1bf880871f08'
assert set(source['effective_cpus'])<=set([4,5,6,7,22,23,24,25])
assert not git('diff','HEAD','--','crates/pg-engine/src')
(out/'source.json').write_text(json.dumps(source,indent=2)+'\n')
env=dict(os.environ,GOLDEN_FILTER='a167_subplan_varied',GOLDEN_PASSWORD='pw')
results=[]
for name,record,want in [('reference',True,0),('engine',False,101)]:
 stage_env=dict(env)
 if record:stage_env['GOLDEN_RECORD']='127.0.0.1:53343'
 cmd=[str(binary),'golden','--exact','--nocapture']
 with (out/(name+'.log')).open('w') as log:
  proc=subprocess.Popen(cmd,cwd=wt,env=stage_env,stdout=log,stderr=subprocess.STDOUT)
  child_cpus=sorted(os.sched_getaffinity(proc.pid))
  code=proc.wait()
 raw=(out/(name+'.log')).read_text()
 row={'stage':name,'command':cmd,'GOLDEN_RECORD':stage_env.get('GOLDEN_RECORD'),'GOLDEN_FILTER':env['GOLDEN_FILTER'],
      'exit':code,'expected_exit':want,'effective_child_cpus':child_cpus,'summaries':re.findall(r'^test result:.*$',raw,re.M),
      'log_sha256':hashlib.sha256((out/(name+'.log')).read_bytes()).hexdigest()}
 results.append(row);(out/'checks.json').write_text(json.dumps(results,indent=2)+'\n');print(json.dumps(row),flush=True)
 if code!=want or not row['summaries']:sys.exit(code or 1)
 assert git('rev-parse','HEAD')==source['head'] and not git('diff','HEAD','--','crates/pg-engine/src')
fixture=wt/'crates/pg-engine/tests/sql/a167_subplan_varied'
for suffix in ['.sql','.out','.actual']:
 p=fixture.with_suffix(suffix)
 if p.exists():(out/p.name).write_bytes(p.read_bytes())
print('PG18.6_REFERENCE_RECORDED_AND_ENGINE_RED_PRESERVED; not a passing engine gate',flush=True)
