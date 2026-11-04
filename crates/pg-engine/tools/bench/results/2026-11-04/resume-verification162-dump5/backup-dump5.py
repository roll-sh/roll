#!/usr/bin/env python3
"""Explicitly store raw blobs via a temporary index; never touch tested HEAD/index/tree."""
from pathlib import Path
import datetime,hashlib,json,os,subprocess,tempfile,time
v=Path(__file__).resolve().parent
w='/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc'
ref='refs/heads/wip/resume-20261104-a162-dump5'
prefix='crates/pg-engine/tools/bench/results/2026-11-04/resume-verification162-dump5/'
def git(*args,input=None,env=None):
 return subprocess.check_output(['git','-C',w,*args],input=input,env=env)
head=git('rev-parse','HEAD').decode().strip()
assert head=='3997cf00a2a301fed65f00469625ff14f223f227'
assert not git('status','--porcelain').strip()
files={}
for name in ['STATUS.md','parity-status.tsv','source-head.txt','status.tsv','roll-engine.sha256','difftest-current.sha256','array-header.sql','compare-regress.py','audit-tap.py','tap-helper-parent-audit.json','backup-dump5.py']:
 p=v/name
 if p.exists():files[name]=p.read_bytes()
for root in ['dump-only-v5']:
 for p in sorted((v/root).rglob('*')):
  if p.is_file() and '__pycache__' not in p.parts and p.suffix in ['.log','.md','.tsv','.txt','.diff','.tap','.sh','.json','.out','.sql','.py']:
   files[str(p.relative_to(v))]=p.read_bytes()
# Preserve actual completed raw TAP streams, including failures. Snapshot state.tsv states completion.
d=Path('/home/meszmate/.cache/a162/pgdump/resume-3997cf00-dump5')
for p in d.glob('tap-*.out'):
 files['dump-only-v5/dump-raw/'+p.name]=p.read_bytes()
manifest={name:hashlib.sha256(body).hexdigest() for name,body in files.items()}
files['SHA256SUMS']=''.join(f'{h}  {n}\n' for n,h in sorted(manifest.items())).encode()
prev=git('ls-remote','origin',ref).decode().strip()
if prev:
 previous=prev.split()[0];git('fetch','-q','origin',ref)
else:previous=None
with tempfile.TemporaryDirectory(dir=v,prefix='dump5-index-') as directory:
 env=dict(os.environ,GIT_INDEX_FILE=directory+'/index')
 git('read-tree',head,env=env)
 for name,body in sorted(files.items()):
  blob=git('hash-object','-w','--stdin',input=body).decode().strip()
  # Explicit cacheinfo insertion includes ignored *.log bytes; .gitignore cannot skip them.
  git('update-index','--add','--cacheinfo','100644',blob,prefix+name,env=env)
 tree=git('write-tree',env=env).decode().strip()
 parents=['-p',head]
 if previous and previous!=head:parents+=['-p',previous]
 commit=git('commit-tree',tree,*parents,input=b'pg-engine: checkpoint setup-only corrected parity supplement with explicit gates\n').decode().strip()
# Independently read every committed blob and verify frozen manifest bytes before pushing.
for name,digest in manifest.items():
 assert hashlib.sha256(git('show',commit+':'+prefix+name)).hexdigest()==digest,name
confirmed=False;attempts=[]
for attempt in range(3):
 pushed=subprocess.run(['git','-C',w,'push','origin',commit+':'+ref],capture_output=True,text=True)
 remote=subprocess.run(['git','-C',w,'ls-remote','origin',ref],capture_output=True,text=True)
 print(pushed.stdout+pushed.stderr,end='')
 attempts.append({'push_exit':pushed.returncode,'ls_remote_exit':remote.returncode,'ls_remote':remote.stdout.strip(),'stderr':remote.stderr.strip()})
 if pushed.returncode==0 and remote.returncode==0 and remote.stdout.split() and remote.stdout.split()[0]==commit:
  confirmed=True;break
 time.sleep(2)
result={'observed_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'tested_head':head,'commit':commit,'ref':ref,'files':len(files),'manifest_hashes':len(manifest),'committed_blobs_verified':True,'confirmed':confirmed,'attempts':attempts}
(v/'dump5-backup.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result))
assert confirmed,'remote checkpoint NOT CONFIRMED'
assert git('rev-parse','HEAD').decode().strip()==head
assert not git('status','--porcelain').strip()
