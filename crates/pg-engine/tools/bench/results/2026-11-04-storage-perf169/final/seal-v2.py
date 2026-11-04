import sys, pathlib as P, subprocess as S, json, os, hashlib as H, shutil, tempfile, time
R=P.Path(__file__).resolve().parent
REPO='/home/meszmate/roll/.claude/worktrees/agent-a76932e8127f6d35b'
BASE='79746f3174996b8047c4c0c30db87f345418b162'
REF='refs/heads/wip/resume-20261104-storage-perf169'
PREFIX='crates/pg-engine/tools/bench/results/2026-11-04-storage-perf169/'
def git(*args,input=None,env=None):return S.check_output(['git','-C',REPO,*args],input=input,env=env)
def sha(b):return H.sha256(b).hexdigest()
def remote():
    out=git('ls-remote','origin',REF).decode().strip();return out.split()[0] if out else None
phase=sys.argv[1];bundle=R/'sealed'/phase;bundle.mkdir(parents=True)
if phase=='preflight':
    paths=[R/x for x in ['PLAN.md','runner.py','build.py','seal.py','NEGATIVE-PASS','negative-sql.cid','commands.jsonl','candidate.before.json','baseline.before.json']]+list(R.glob('*-negative-*.log'))
elif phase=='pretime':
    paths=[R/x for x in ['runner-v2.py','runner-v3.py','launch.py','launch-v3.py','LAUNCH.md','PLAN-v2.md','PLAN-v3.md','cleanup-control.py','cleanup-controls.json','CLEANUP-CONTROLS-PASS','seal-v2.py']]+list(R.glob('cleanup-control-*.log'))
elif phase=='build':
    assert (R/'BUILD-PASS').exists()
    paths=[R/x for x in ['builds.json','BUILD-PASS','heavy-build.log','build-candidate.log','build-baseline.log','candidate.before.json','baseline.before.json','candidate.after.json','baseline.after.json']]
else:
    paths=[p for p in R.iterdir() if p.is_file() and p.suffix not in ['.data'] and p.name not in ['owner-commit','seal.lock'] and not p.name.startswith('push-receipt-')]
for path in paths:
    assert path.is_file(),path
    shutil.copyfile(path,bundle/path.name)
manifest={p.name:dict(sha256=sha(p.read_bytes()),bytes=p.stat().st_size) for p in sorted(bundle.iterdir())}
(bundle/'MANIFEST.json').write_text(json.dumps(dict(phase=phase,closed=True,scope='every bundle file except this manifest; no files may be added',files=manifest),indent=2)+'\n')
assert set(p.name for p in bundle.iterdir())==set(manifest)|{'MANIFEST.json'}
for p in bundle.iterdir():p.chmod(0o444)
bundle.chmod(0o555)
old=remote();local=(R/'owner-commit').read_text().strip() if (R/'owner-commit').exists() else None
assert old==local,(old,local)
parent=local or BASE
idx=R/('temp-index-'+phase);assert not idx.exists()
env=os.environ.copy();env['GIT_INDEX_FILE']=str(idx)
git('read-tree',parent,env=env)
entries=[]
for path in sorted(bundle.iterdir()):
    data=path.read_bytes();blob=git('hash-object','-w','--stdin',input=data).decode().strip();dest=PREFIX+phase+'/'+path.name
    git('update-index','--add','--cacheinfo','100644',blob,dest,env=env);entries.append((dest,blob,sha(data)))
tree=git('write-tree',env=env).decode().strip()
commit=git('commit-tree',tree,'-p',parent,input=('storage perf169: sealed '+phase+' evidence\n').encode()).decode().strip()
changed=git('diff','--name-only',parent,commit).decode().splitlines()
assert changed and all(x.startswith(PREFIX+phase+'/') for x in changed),changed
for dest,blob,digest in entries:
    actual=git('rev-parse',commit+':'+dest).decode().strip();assert actual==blob
    assert sha(git('cat-file','blob',actual))==digest
# Check the complete cumulative diff against the measured source before every push.
allpaths=git('diff','--name-only',BASE,commit).decode().splitlines();assert all(x.startswith(PREFIX) for x in allpaths)
res=S.run(['git','-C',REPO,'push','origin',commit+':'+REF],capture_output=True,text=True)
receipt=dict(phase=phase,commit=commit,parent=parent,tree=tree,ref=REF,changed=changed,all_paths=allpaths,entries=entries,push_exit=res.returncode,push_stdout=res.stdout,push_stderr=res.stderr,remote=remote())
(R/('push-receipt-'+phase+'.json')).write_text(json.dumps(receipt,indent=2)+'\n')
assert res.returncode==0 and receipt['remote']==commit,receipt
(R/'owner-commit').write_text(commit+'\n');idx.unlink()
print(json.dumps(dict(phase=phase,commit=commit,remote=receipt['remote'],manifest=sha((bundle/'MANIFEST.json').read_bytes()),files=len(manifest))))
