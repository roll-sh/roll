import os,subprocess,json,hashlib,tempfile,sys,time
from pathlib import Path
r=Path('/home/meszmate/roll/.claude/worktrees/agent-a3512214927146502');cache=Path('/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/index163_followup')
label=sys.argv[1];final='final' in label;prefix='crates/pg-engine/results/2026-11-04/resume-owner163-review-'+label;ref='refs/heads/wip/resume-20261104-a163-review-'+label

def git(*a,data=None,env=None):return subprocess.run(['git','-C',str(r),*a],input=data,env=env,capture_output=True,check=True).stdout
head=git('rev-parse','HEAD').strip();idx=Path(git('rev-parse','--git-path','index').decode().strip());oldidx=idx.read_bytes();assert not git('ls-remote','origin',ref).strip()
files={}
for p in sorted(cache.rglob('*')):
 if not p.is_file() or p.suffix=='.pyc' or '__pycache__' in p.parts:continue
 rel=p.relative_to(cache)
 if not final and rel.parts[0].startswith('fixed'):continue
 if p.name.endswith('-audit.json'):continue
 files[prefix+'/'+str(rel)]=p.read_bytes()
# Exact PostgreSQL oracle identity and OCI invocation/mount inventory (no host environment).
meta=subprocess.check_output(['docker','inspect','a163-iref','--format','{{json .}}']);d=json.loads(meta)
identity={'container_id':d['Id'],'image_id':d['Image'],'image_name':d['Config']['Image'],'command':d['Config']['Cmd'],'mounts':d['Mounts'],'network_mode':d['HostConfig']['NetworkMode'],'version_command':['docker','exec','a163-iref','psql','-p','53263','-U','postgres','-Atc','select version();'],'version_output':subprocess.check_output(['docker','exec','a163-iref','psql','-p','53263','-U','postgres','-Atc','select version();']).decode()}
files[prefix+'/reference-identity.json']=json.dumps(identity,indent=2).encode()
manifest={'source_head':head.decode(),'status':'EXACT COMMAND OUTCOMES IN results.json; prior failures retained; no performance/combined-service claim','files':[{'path':p,'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest()} for p,b in sorted(files.items())]}
files[prefix+'/manifest.json']=json.dumps(manifest,indent=2).encode()
with tempfile.TemporaryDirectory() as td:
 env={**os.environ,'GIT_INDEX_FILE':td+'/index'};git('read-tree',head.decode(),env=env)
 for path,b in files.items():
  blob=git('hash-object','-w','--stdin',data=b).strip().decode();git('update-index','--add','--cacheinfo','100644',blob,path,env=env)
 tree=git('write-tree',env=env).strip().decode();commit=git('commit-tree',tree,'-p',head.decode(),data=('a163 immutable review evidence '+label+'\n').encode()).strip().decode()
 changed=git('diff-tree','--no-commit-id','--name-only','-r',head.decode(),commit).decode().splitlines();assert set(changed)==set(files)
 for path,b in files.items():assert hashlib.sha256(git('show',commit+':'+path)).digest()==hashlib.sha256(b).digest(),path
 assert git('rev-parse','HEAD').strip()==head and idx.read_bytes()==oldidx
 for retry in range(3):
  try:git('push','origin',commit+':'+ref);break
  except subprocess.CalledProcessError:
   if retry==2:raise
   time.sleep(3)
 remote=git('ls-remote','origin',ref).decode().split()[0];assert remote==commit
 audit={'commit':commit,'ref':ref,'payloads':len(files)-1,'manifest':prefix+'/manifest.json','committed_blob_sha256_verified':True,'head_index_unchanged':True,'remote':remote}
 (cache/(label+'-audit.json')).write_text(json.dumps(audit,indent=2));print(json.dumps(audit))
