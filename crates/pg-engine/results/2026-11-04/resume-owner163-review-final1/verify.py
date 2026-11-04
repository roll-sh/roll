import os,subprocess,json,hashlib,re,time
from pathlib import Path
r=Path('/home/meszmate/roll/.claude/worktrees/agent-a3512214927146502');o=Path('/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/index163_followup/fixed');o.mkdir()
e={**os.environ,'PATH':'/home/meszmate/.cargo/bin:'+os.environ['PATH'],'CARGO_TARGET_DIR':'/home/meszmate/.cache/a163/target','TMPDIR':'/home/meszmate/.cache/a163/tmp'}
def pins():
 paths=[p for p in (r/'crates/pg-engine/src').rglob('*.rs')]+[p for p in (r/'crates/pg-engine/tests').rglob('*') if p.is_file() and p.suffix in ('.rs','.sql','.out','.spec')]+[r/'Cargo.toml',r/'Cargo.lock',r/'crates/pg-engine/Cargo.toml']
 return {str(p.relative_to(r)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(paths)}
head=subprocess.check_output(['git','-C',str(r),'rev-parse','HEAD']);before=pins();(o/'source-head').write_bytes(head);(o/'source-sha256.json').write_text(json.dumps(before,indent=2));(o/'source-status').write_bytes(subprocess.check_output(['git','-C',str(r),'status','--porcelain']))
steps=[('build',['cargo','build','-p','pg-engine','--bin','roll-engine'],{},None),('focused',['cargo','test','-p','pg-engine','--lib','--test','golden','--test','big_tables','--test','deep_input'],{'GOLDEN_FILTER':'a163_'},[(184,0,2),(15,0,0),(3,0,0),(3,0,0)]),('clippy',['cargo','clippy','-p','pg-engine','--all-targets'],{},None),('release-golden',['cargo','test','--release','-p','pg-engine','--test','golden'],{'GOLDEN_FILTER':'a163_review_sizes'},[(3,0,0)]),('bitmap-isolation',['cargo','test','-p','pg-engine','--test','pg_isolation','--','--ignored','--nocapture'],{'PGISOLATION_ONLY':'roll-bitmap-recheck','PGISOLATION_DIR':'/home/meszmate/.cache/a163/v/iso','PGISOLATION_PORT':'53269','PGISOLATION_REF':'127.0.0.1:53263','PGISOLATION_DB':'a163_bitmap_followup','PGISOLATION_REPORT_DIR':str(o/'isolation-report')},[(1,0,0)])]
results=[]
for name,args,env,expected in steps:
 assert pins()==before and subprocess.check_output(['git','-C',str(r),'rev-parse','HEAD'])==head
 (o/(name+'.command.json')).write_text(json.dumps({'argv':args,'env':env,'cwd':str(r),'source_head':head.decode().strip()},indent=2))
 with (o/(name+'.log')).open('wb') as f:p=subprocess.run(args,cwd=r,env={**e,**env},stdout=f,stderr=subprocess.STDOUT)
 log=(o/(name+'.log')).read_text(); summaries=[tuple(map(int,v)) for v in re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored',log)]
 results.append({'name':name,'exit':p.returncode,'summaries':summaries,'expected':expected});(o/'results.json').write_text(json.dumps(results,indent=2));assert p.returncode==0,(name,p.returncode)
 assert not re.search(r'^error|test result: FAILED|panicked at',log,re.M),name
 if expected:assert summaries==expected,(name,summaries,expected)
 if name=='bitmap-isolation':
  assert 'pg_isolation: 1/1 specs, 3/3 permutations' in log,log[-1500:]
  outputs=list(Path('/home/meszmate/.cache/a163/v/iso/out').rglob('*roll-bitmap-recheck*'))
  (o/'isolation-output-paths.json').write_text(json.dumps([str(x) for x in outputs],indent=2))
 assert pins()==before
(o/'complete').write_text('All declared per-command exits, summaries, source hashes verified; no combined service/performance claim\n')
