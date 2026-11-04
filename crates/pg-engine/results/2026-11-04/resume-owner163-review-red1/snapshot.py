import subprocess,os,tempfile,hashlib,json,sys,time
from pathlib import Path
root=Path('/home/meszmate/roll/.claude/worktrees/agent-a3512214927146502')
label=sys.argv[1]; ref='refs/heads/wip/resume-20261104-a163-'+label

def git(*a,data=None,env=None):
 return subprocess.run(['git','-C',str(root),*a],input=data,env=env,capture_output=True,check=True).stdout
head=git('rev-parse','HEAD').strip(); index=Path(git('rev-parse','--git-path','index').decode().strip()); before=index.read_bytes()
assert not git('ls-remote','origin',ref).strip()
with tempfile.TemporaryDirectory() as td:
 env={**os.environ,'GIT_INDEX_FILE':td+'/index'}
 git('read-tree',head.decode(),env=env)
 git('add','-A',env=env)
 evidence=root/'crates/pg-engine/results/2026-11-04/resume-owner163'
 git('add','-f',str(evidence),env=env)
 tree=git('write-tree',env=env).strip()
 commit=git('commit-tree',tree.decode(),'-p',head.decode(),data=('a163 '+label+' owner checkpoint\n').encode()).strip().decode()
 for p in evidence.rglob('*'):
  if p.is_file(): assert hashlib.sha256(git('show',commit+':'+str(p.relative_to(root)))).digest()==hashlib.sha256(p.read_bytes()).digest(),p
 assert git('rev-parse','HEAD').strip()==head and index.read_bytes()==before
 for retry in range(3):
  try: git('push','origin',commit+':'+ref); break
  except subprocess.CalledProcessError:
   if retry==2: raise
   time.sleep(3)
 assert git('ls-remote','origin',ref).split()[0].decode()==commit
 print(json.dumps({'commit':commit,'ref':ref,'head_unchanged':head.decode(),'evidence_blobs_verified':True}))
