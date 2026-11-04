#!/usr/bin/env python3
import hashlib,json,os,subprocess,time
from pathlib import Path
W='/home/meszmate/roll/.claude/worktrees/agent-a640c14a8f76f8ea4';D=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner/bytea165compare');D.mkdir(exist_ok=True)
assert not (D/'results.json').exists()
env=dict(os.environ,CARGO_TARGET_DIR='/home/meszmate/.cache/a165/target',TMPDIR='/home/meszmate/.cache/a165/tmp')
def g(*args):return subprocess.check_output(['git','-C',W,*args],text=True).strip()
assert g('rev-parse','HEAD')=='ab3f64b8dd92203123f6777ae4a6028d3a3aae67' and not g('status','--porcelain')
rows=[]
try:
 for name,ref in [('candidate-before','ab3f64b8'),('shared-base','fe7b0023'),('candidate-after','ab3f64b8')]:
  if name=='shared-base':g('switch','--detach',ref)
  elif name=='candidate-after':g('switch','work-a165-independent-20261104')
  head=g('rev-parse','HEAD');tree=g('rev-parse','HEAD^{tree}')
  cmd=['cargo','test','-p','pg-engine','--test','wire','big_bytea_values_go_out_in_linear_time','--','--exact','--nocapture']
  (D/(name+'.command')).write_text(' '.join(cmd)+'\n');(D/(name+'.source')).write_text(head+' '+tree+'\n')
  before=time.monotonic()
  with (D/(name+'.log')).open('w') as out:p=subprocess.run(cmd,cwd=W,env=env,stdout=out,stderr=subprocess.STDOUT)
  log=(D/(name+'.log')).read_text();assert not ('could not compile' in log or 'error[E' in log),log[-3000:]
  assert 'test result:' in log
  assert g('rev-parse','HEAD')==head and not g('status','--porcelain')
  rows.append(dict(name=name,head=head,tree=tree,exit=p.returncode,wall_including_cargo=time.monotonic()-before,wire_source_sha256=hashlib.sha256(Path(W,'crates/pg-engine/tests/wire.rs').read_bytes()).hexdigest()))
  (D/'results.json').write_text(json.dumps(rows,indent=2)+'\n');print(json.dumps(rows[-1]),flush=True)
finally:
 g('switch','work-a165-independent-20261104')
 assert g('rev-parse','HEAD')=='ab3f64b8dd92203123f6777ae4a6028d3a3aae67' and not g('status','--porcelain')
 (D/'RESTORED').write_text(g('rev-parse','HEAD')+'\n')
