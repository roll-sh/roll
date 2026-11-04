import datetime,hashlib,json,os,pathlib,re,subprocess,sys
root=pathlib.Path('/home/meszmate/roll/.claude/worktrees/agent-ac9369af470e262d2')
out=pathlib.Path(__file__).parent/'gate1';out.mkdir(exist_ok=True)
expected=sys.argv[1]
def git(*args):return subprocess.check_output(['git','-C',str(root),*args],text=True)
def identity():
 files=git('ls-files','crates/pg-engine/src','crates/pg-engine/tests','Cargo.toml','Cargo.lock','crates/pg-engine/Cargo.toml').splitlines()
 return {f:hashlib.sha256((root/f).read_bytes()).hexdigest() for f in files}
assert git('rev-parse','HEAD').strip()==expected
assert not git('diff','256e4d04','HEAD','--','crates/pg-engine/src','Cargo.toml','Cargo.lock','crates/pg-engine/Cargo.toml')
before=identity();(out/'source-head').write_text(expected+'\n');(out/'source-sha256.json').write_text(json.dumps(before,indent=2)+'\n')
(out/'source-status-before').write_text(git('status','--porcelain=v1'))
env=dict(os.environ,CARGO_BUILD_JOBS='2',RUST_TEST_THREADS='2',CARGO_PROFILE_DEV_DEBUG='line-tables-only',CARGO_PROFILE_TEST_DEBUG='line-tables-only',CARGO_TARGET_DIR='/home/meszmate/.cache/a166/target-debug',TMPDIR='/home/meszmate/.cache/a166/tmp',GOLDEN_FILTER='a166_')
env['PATH']='/home/meszmate/.cargo/bin:'+env['PATH']
steps=[('debug',['cargo','test','-p','pg-engine','--test','catalog_lazy_snapshot','--test','golden','--','--nocapture']),('release',['cargo','test','--release','-p','pg-engine','--test','catalog_lazy_snapshot','--test','golden','--','--nocapture']),('clippy',['cargo','clippy','-p','pg-engine','--all-targets'])];results=[]
for name,cmd in steps:
 assert identity()==before and git('rev-parse','HEAD').strip()==expected
 step_env=dict(env)
 if name=='release':step_env.update(CARGO_TARGET_DIR='/home/meszmate/.cache/a166/target-release',CARGO_PROFILE_RELEASE_DEBUG='line-tables-only')
 (out/(name+'.command.json')).write_text(json.dumps({'argv':cmd,'cwd':str(root),'env':{k:v for k,v in step_env.items() if k.startswith(('CARGO','GOLDEN','RUST_TEST'))}},indent=2)+'\n')
 print('START',name,datetime.datetime.now(datetime.timezone.utc).isoformat(),flush=True)
 with (out/(name+'.log')).open('w') as f:rc=subprocess.run(cmd,cwd=root,env=step_env,stdout=f,stderr=subprocess.STDOUT).returncode
 log=(out/(name+'.log')).read_text(errors='replace');summaries=re.findall(r'^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$',log,re.M)
 ok=rc==0
 if name!='clippy':
  ok=ok and sorted(int(p) for s,p,f,i in summaries)==[1,3] and all(s=='ok' and f=='0' and i=='0' for s,p,f,i in summaries)
  ok=ok and log.count('4 golden files match')==3
  ok=ok and bool(re.search(r'inner join restricted generations: [1-9][0-9]*',log)) and 'count_rest whole catalog generations: 0' in log
 else:
  old=pathlib.Path('/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/catalog166_review/fixed-gates/clippy.log').read_text()
  pattern=r'^warning: ([^\n]+)\n\s*--> ([^\n]+)'
  prior=sorted(re.findall(pattern,old,re.M));current=sorted(re.findall(pattern,log,re.M))
  (out/'clippy-warning-comparison.json').write_text(json.dumps({'baseline':prior,'current':current,'added':sorted(set(current)-set(prior))},indent=2)+'\n')
  ok=ok and not (set(current)-set(prior))
 row={'name':name,'exit':rc,'validated':ok,'summaries':summaries,'sha256':hashlib.sha256(log.encode()).hexdigest()};results.append(row)
 (out/'results.json').write_text(json.dumps(results,indent=2)+'\n');print(json.dumps(row),flush=True);print('\n'.join(log.splitlines()[-12:]),flush=True)
 assert rc==0 and ok,'FAILED '+name
 assert identity()==before and git('rev-parse','HEAD').strip()==expected
(out/'source-status-after').write_text(git('status','--porcelain=v1'))
(out/'complete').write_text('Three declared steps passed; source hashes and HEAD unchanged.\n')
