import importlib.util,pathlib as P,subprocess as S,sys,json
R=P.Path(__file__).resolve().parent

def child(version,mode):
 spec=importlib.util.spec_from_file_location('runner',R/('runner-'+version+'.py'));m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
 cid='0'*64;other='1'*64;m.OWN=[cid];calls=[]
 def inspect(ident):
  assert ident==cid
  return dict(Config=dict(Labels={'agent.worktree':m.LABEL}),State=dict(Running=True))
 def run(cmd,tag,**kw):
  assert cmd[:2] in [['docker','stop'],['docker','logs'],['docker','rm']]
  assert cmd[-1]==cid and other not in cmd
  calls.append(cmd)
  if mode=='failure' and cmd[1]=='rm':raise RuntimeError('injected exact-ID Docker removal failure')
  return '',0
 m.inspect=inspect;m.run=run
 try:
  m.cleanup()
 except Exception as e:
  print(json.dumps(dict(version=version,mode=mode,exit=1,remaining=m.OWN,calls=calls,error=str(e))));return 1
 print(json.dumps(dict(version=version,mode=mode,exit=0,remaining=m.OWN,calls=calls)));return 0
if len(sys.argv)>1:sys.exit(child(*sys.argv[1:]))
results=[]
for version,mode,want in [('v2','failure',0),('v3','failure',1),('v3','success',0)]:
 q=S.run([sys.executable,__file__,version,mode],capture_output=True,text=True)
 (R/('cleanup-control-'+version+'-'+mode+'.log')).write_text(q.stdout+q.stderr)
 assert q.returncode==want,(q.returncode,want,q.stdout,q.stderr)
 obj=json.loads(q.stdout.strip().splitlines()[-1]);assert bool(obj['remaining'])==(mode=='failure')
 results.append(obj)
(R/'cleanup-controls.json').write_text(json.dumps(results,indent=2)+'\n')
(R/'CLEANUP-CONTROLS-PASS').write_text('v2 masks removal failure exit0 + retained ID; v3 fails exit1 + retained ID; v3 successful exact-ID cleanup exit0 + empty ownership\n')
print(json.dumps(results,indent=2))
