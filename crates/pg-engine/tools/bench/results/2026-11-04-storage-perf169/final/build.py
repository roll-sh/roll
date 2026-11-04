import subprocess as s, pathlib as p, hashlib as h, json, os, sys, time
R=p.Path(__file__).resolve().parent
SIDES=[('candidate','/home/meszmate/roll/.claude/worktrees/agent-a76932e8127f6d35b','79746f3174996b8047c4c0c30db87f345418b162','/home/meszmate/.cache/a169/target'),('baseline','/home/meszmate/.cache/a169/base','5c31223568fead896be10e66edd59e3d5a261002','/home/meszmate/.cache/a169/target-base')]
def sha(f): return h.sha256(p.Path(f).read_bytes()).hexdigest()
def git(src,*a): return s.check_output(['git','-C',src,*a])
def snapshot(src):
    files=git(src,'ls-files','-z').decode().split('\0')[:-1]
    idx=git(src,'rev-parse','--git-path','index').decode().strip()
    if not p.Path(idx).is_absolute(): idx=str(p.Path(src)/idx)
    return dict(head=git(src,'rev-parse','HEAD').decode().strip(),index=sha(idx),status=git(src,'status','--porcelain=v1','--untracked-files=all').decode(),files={f:sha(p.Path(src)/f) for f in files if (p.Path(src)/f).is_file()})
def stable():
    for name,src,commit,target in SIDES:
        before=json.loads((R/f'{name}.before.json').read_text()); now=snapshot(src)
        assert now==before, f'SOURCE MUTATED {name}'
        assert now['head']==commit
    if (R/'builds.json').exists():
        for x in json.loads((R/'builds.json').read_text()): assert sha(x['binary'])==x['sha256']
if __name__=='__main__':
  out=[]
  for name,src,commit,target in SIDES:
    before=snapshot(src); assert before['head']==commit
    assert not git(src,'diff','--name-only').strip() and not git(src,'diff','--cached','--name-only').strip()
    (R/f'{name}.before.json').write_text(json.dumps(before,indent=2))
  for name,src,commit,target in SIDES:
    cmd=['cargo','build','--locked','--profile','dist','-p','pg-engine','--bin','roll-engine']
    env=os.environ.copy();env.update(CARGO_TARGET_DIR=target,CARGO_BUILD_JOBS='2')
    t=time.time()
    with (R/f'build-{name}.log').open('xb') as log: rc=s.call(cmd,cwd=src,env=env,stdout=log,stderr=s.STDOUT)
    x=dict(side=name,source=src,commit=commit,target=target,command=cmd,exit=rc,wall=time.time()-t)
    if rc==0: x.update(binary=target+'/dist/roll-engine',sha256=sha(target+'/dist/roll-engine'))
    out.append(x);(R/'builds.json').write_text(json.dumps(out,indent=2));stable()
    (R/f'{name}.after.json').write_text(json.dumps(snapshot(src),indent=2))
    assert rc==0,x
  (R/'BUILD-PASS').write_text('both dist builds and exact source/binary mappings pass\n')
