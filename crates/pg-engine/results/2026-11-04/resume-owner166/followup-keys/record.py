import hashlib,json,os,pathlib,subprocess
r=pathlib.Path('/home/meszmate/roll/.claude/worktrees/agent-ac9369af470e262d2')
o=pathlib.Path(__file__).parent/'record';o.mkdir(exist_ok=True)
b='/home/meszmate/.cache/a166/target-debug/debug/deps/golden-9749ad749af5ca1c'
assert subprocess.check_output(['git','-C',str(r),'rev-parse','HEAD'],text=True).strip()=='0f077b8d01577f92085494a7c3c23f2c62e3fb09'
cmd=[b,'--exact','golden','--nocapture'];env=dict(os.environ,GOLDEN_RECORD='127.0.0.1:53320',GOLDEN_PASSWORD='pw',GOLDEN_FILTER='a166_lazy_keys_count')
(o/'command.json').write_text(json.dumps({'argv':cmd,'env':{k:v for k,v in env.items() if k.startswith('GOLDEN_')},'binary_sha256':hashlib.sha256(pathlib.Path(b).read_bytes()).hexdigest()},indent=2)+'\n')
with (o/'raw.log').open('w') as f:rc=subprocess.run(cmd,cwd=r,env=env,stdout=f,stderr=subprocess.STDOUT).returncode
log=(o/'raw.log').read_text();result={'exit':rc,'recorded':('recorded ' in log),'expected_summary':('test result: ok. 1 passed; 0 failed;' in log)}
(o/'result.json').write_text(json.dumps(result,indent=2)+'\n')
assert rc==0 and result['recorded'] and result['expected_summary'],result
print(json.dumps(result),flush=True)
