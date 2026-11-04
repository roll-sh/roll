#!/usr/bin/env python3
"""Export a closed evidence set using a private index; never touch the checkout."""
import pathlib,sys,subprocess,tempfile,os,json,hashlib,datetime
cfg=json.loads(pathlib.Path(sys.argv[1]).read_text());repo=cfg['repo'];prefix=cfg['prefix'].rstrip('/')+'/'
def git(*args,data=None,env=None):return subprocess.check_output(['git','-C',repo,*args],input=data,env=env)
head=git('rev-parse','HEAD');status=git('status','--porcelain');index=git('write-tree')
remote=git('ls-remote','origin',cfg['ref']).decode().split();assert remote and remote[0]==cfg['parent'],remote
files={}
for item in cfg['files']:
 src=pathlib.Path(item['source']);assert src.is_file(),src
 assert item['name'] not in files,item['name'];files[item['name']]=src.read_bytes()
assert 'SHA256SUMS' not in files
files['SHA256SUMS']=''.join(hashlib.sha256(v).hexdigest()+'  '+k+'\n' for k,v in sorted(files.items())).encode()
with tempfile.TemporaryDirectory(prefix='parent-evidence-index-',dir=cfg['cache']) as tmp:
 env=dict(os.environ,GIT_INDEX_FILE=tmp+'/index');git('read-tree',cfg['parent'],env=env)
 assert not git('ls-tree','-r','--name-only',cfg['parent'],'--',prefix).strip(),'prefix already exists'
 for name,data in sorted(files.items()):
  blob=git('hash-object','-w','--stdin',data=data).decode().strip();git('update-index','--add','--cacheinfo','100644',blob,prefix+name,env=env)
 tree=git('write-tree',env=env).decode().strip();commit=git('commit-tree',tree,'-p',cfg['parent'],data=(cfg['message']+'\n').encode()).decode().strip()
 names=git('ls-tree','-r','--name-only',commit,'--',prefix).decode().splitlines();assert set(names)=={prefix+n for n in files}
 for name,data in files.items():assert git('show',commit+':'+prefix+name)==data,name
 subprocess.run(['git','-C',repo,'push','origin',commit+':'+cfg['ref']],check=True)
 assert git('ls-remote','origin',cfg['ref']).decode().split()[0]==commit
 assert git('rev-parse','HEAD')==head and git('status','--porcelain')==status and git('write-tree')==index,'checkout changed'
result={'time':datetime.datetime.now(datetime.timezone.utc).isoformat(),'commit':commit,'ref':cfg['ref'],'prefix':prefix,'files':len(files),'hashes':len(files)-1,'all_committed_bytes_verified':True,'closed_membership':True,'remote_exact':True,'head_index_worktree_unchanged':True}
pathlib.Path(cfg['receipt']).write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result))
