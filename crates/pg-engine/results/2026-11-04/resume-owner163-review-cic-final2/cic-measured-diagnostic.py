import subprocess,os,json
from pathlib import Path
r='/home/meszmate/roll/.claude/worktrees/agent-a3512214927146502';o=Path('/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/index163_followup/cic-measured-diagnostic');o.mkdir()
e={**os.environ,'PATH':'/home/meszmate/.cargo/bin:'+os.environ['PATH'],'CARGO_TARGET_DIR':'/home/meszmate/.cache/a163/target','TMPDIR':'/home/meszmate/.cache/a163/tmp','RUST_BACKTRACE':'1'}
a=['cargo','test','-p','pg-engine','--test','big_tables','--','--exact','measured_serial_include_size','--nocapture'];(o/'command.json').write_text(json.dumps({'argv':a,'env':{k:e[k] for k in ['CARGO_TARGET_DIR','RUST_BACKTRACE']},'source':subprocess.check_output(['git','-C',r,'rev-parse','HEAD']).decode()},indent=2))
with (o/'diagnostic.log').open('wb') as f:p=subprocess.run(a,cwd=r,env=e,stdout=f,stderr=subprocess.STDOUT)
(o/'exit.json').write_text(json.dumps({'exit':p.returncode,'status':'FAILED diagnostic; expected reproduction not passing source gate'}));assert p.returncode==101
