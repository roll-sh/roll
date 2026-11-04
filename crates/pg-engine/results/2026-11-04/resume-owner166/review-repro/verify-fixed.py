import datetime, hashlib, json, os, pathlib, re, subprocess, sys
root=pathlib.Path('/home/meszmate/roll/.claude/worktrees/agent-ac9369af470e262d2')
out=pathlib.Path('/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/catalog166_review/fixed-gates')
out.mkdir(exist_ok=True)
expected='256e4d049d4f82db1166c08437bab736a0d0a3ee'
def git(*args): return subprocess.check_output(['git','-C',str(root),*args],text=True)
def identity():
 files=git('ls-files','crates/pg-engine/src','crates/pg-engine/tests','Cargo.toml','Cargo.lock','crates/pg-engine/Cargo.toml').splitlines()
 return {f:hashlib.sha256((root/f).read_bytes()).hexdigest() for f in files}
assert git('rev-parse','HEAD').strip()==expected
before=identity()
(out/'source-head').write_text(expected+'\n')
(out/'source-status-before').write_text(git('status','--porcelain=v1'))
(out/'source-sha256.json').write_text(json.dumps(before,indent=2)+'\n')
env=dict(os.environ,CARGO_BUILD_JOBS='2',RUST_TEST_THREADS='2',CARGO_PROFILE_DEV_DEBUG='line-tables-only',CARGO_PROFILE_TEST_DEBUG='line-tables-only',CARGO_TARGET_DIR='/home/meszmate/.cache/a166/target-debug',TMPDIR='/home/meszmate/.cache/a166/tmp',GOLDEN_FILTER='a166_')
env['PATH']='/home/meszmate/.cargo/bin:'+env['PATH']
steps=[
 ('debug-golden',['cargo','test','-p','pg-engine','--test','golden','--','--nocapture'],[3]),
 ('debug-catalog-kept',['cargo','test','-p','pg-engine','--test','catalog_format','--test','catalog_storage','--test','kept_plans','--','--nocapture'],[4,6,7]),
 ('debug-catalog-unit',['cargo','test','-p','pg-engine','--lib','catalog::','--','--nocapture'],None),
 ('build',['cargo','build','-p','pg-engine','--bin','roll-engine'],[]),
 ('clippy',['cargo','clippy','-p','pg-engine','--all-targets'],[]),
 ('release-golden',['cargo','test','--release','-p','pg-engine','--test','golden','--','--nocapture'],[3]),
]
results=[]
for name,cmd,counts in steps:
 assert identity()==before, 'SOURCE CHANGED BEFORE '+name
 assert git('rev-parse','HEAD').strip()==expected
 step_env=dict(env)
 if name.startswith('release'):
  step_env.update(CARGO_TARGET_DIR='/home/meszmate/.cache/a166/target-release',CARGO_PROFILE_RELEASE_DEBUG='line-tables-only')
 print('START',name,datetime.datetime.now(datetime.timezone.utc).isoformat(),flush=True)
 (out/(name+'.command.json')).write_text(json.dumps({'cwd':str(root),'argv':cmd,'env':{k:v for k,v in step_env.items() if k.startswith(('CARGO','RUST_TEST','GOLDEN'))}},indent=2)+'\n')
 with (out/(name+'.log')).open('w') as f:
  rc=subprocess.run(cmd,cwd=root,env=step_env,stdout=f,stderr=subprocess.STDOUT).returncode
 log=(out/(name+'.log')).read_text(errors='replace')
 summaries=re.findall(r'^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$',log,re.M)
 ok=rc==0
 if counts!=[]:
  ok=ok and bool(summaries) and all(s=='ok' and int(p)>0 and int(f)==0 for s,p,f,i in summaries)
  if counts is not None: ok=ok and sorted(int(p) for s,p,f,i in summaries)==sorted(counts)
 if 'golden' in name:
  ok=ok and log.count('3 golden files match')==3
 row={'name':name,'exit':rc,'validated':ok,'summaries':summaries,'log_sha256':hashlib.sha256(log.encode()).hexdigest()}
 results.append(row)
 (out/'results.json').write_text(json.dumps(results,indent=2)+'\n')
 print(json.dumps(row),flush=True)
 print('\n'.join(log.splitlines()[-14:]),flush=True)
 assert rc==0 and ok,'FAILED '+name
 assert identity()==before, 'SOURCE CHANGED AFTER '+name
assert git('rev-parse','HEAD').strip()==expected
(out/'source-status-after').write_text(git('status','--porcelain=v1'))
(out/'complete').write_text('ALL DECLARED STEPS PASSED; source identity unchanged\n')
