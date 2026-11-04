import os,subprocess,json,hashlib
from pathlib import Path
r=Path('/home/meszmate/roll/.claude/worktrees/agent-a3512214927146502'); o=Path('/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/index163_followup/cic-red');o.mkdir()
e={**os.environ,'PATH':'/home/meszmate/.cargo/bin:'+os.environ['PATH'],'CARGO_TARGET_DIR':'/home/meszmate/.cache/a163/target','TMPDIR':'/home/meszmate/.cache/a163/tmp','GOLDEN_FILTER':'a163_review_concurrent'}
(o/'source-head').write_bytes(subprocess.check_output(['git','-C',str(r),'rev-parse','HEAD']))
(o/'source.diff').write_bytes(subprocess.check_output(['git','-C',str(r),'diff','HEAD']))
results=[]
for name,extra,args,expected in [('pg18-record',{'GOLDEN_RECORD':'127.0.0.1:53263'},['cargo','test','-p','pg-engine','--test','golden','--','golden','--exact','--nocapture'],0),('engine-red',{},['cargo','test','-p','pg-engine','--test','golden','--','golden','--exact','--nocapture'],101)]:
 with (o/(name+'.log')).open('wb') as f: p=subprocess.run(args,cwd=r,env={**e,**extra},stdout=f,stderr=subprocess.STDOUT)
 log=(o/(name+'.log')).read_text(); results.append({'name':name,'command':args,'env':extra,'exit':p.returncode,'expected':expected}); (o/'results.json').write_text(json.dumps(results,indent=2))
 assert p.returncode==expected,(name,p.returncode)
 assert ('test result: ok. 1 passed' if expected==0 else 'test result: FAILED. 0 passed; 1 failed') in log
 if expected: 
  for pth in (r/'crates/pg-engine/tests/sql').glob('a163_review_concurrent*'):
   (o/pth.name).write_bytes(pth.read_bytes())
(o/'complete').write_text('PG18.6 recorded; engine mismatch reproduced, not pass\n')
