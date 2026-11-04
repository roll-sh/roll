#!/usr/bin/env python3
"""Hash artifacts named in completed stage logs; omit nonexistent paths explicitly."""
import hashlib,json,re,sys
from pathlib import Path
root=Path(sys.argv[1]); checks=json.loads((root/'checks.json').read_text()); artifacts={}
for c in checks:
 text=(root/(c['name']+'.log')).read_text(errors='replace')
 for path in re.findall(r'^\s*(?:Running|Doc-tests).*?\((/[^)]+)\)',text,re.M):
  p=Path(path)
  if p.exists():
   before=p.stat();h=hashlib.sha256()
   with p.open('rb') as f:
    for chunk in iter(lambda:f.read(1<<20),b''):h.update(chunk)
   after=p.stat();assert(before.st_size,before.st_mtime_ns)==(after.st_size,after.st_mtime_ns)
   artifacts[path]={'sha256':h.hexdigest(),'bytes':after.st_size,'mtime_ns':after.st_mtime_ns,'stage':c['name']}
  else:artifacts[path]={'missing':True,'stage':c['name']}
plan_files=list(root.parent.glob(root.name+'.json'))
if plan_files:
 plan=json.loads(plan_files[0].read_text());target=Path(plan.get('env',{}).get('CARGO_TARGET_DIR',''))
 for c in checks:
  cmd=c['command']
  if c['pass'] and cmd[:2]==['cargo','build'] and '--bin' in cmd:
   path=target/('release' if '--release' in cmd else 'debug')/cmd[cmd.index('--bin')+1]
   if path.is_file():
    artifacts[str(path)]={'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'bytes':path.stat().st_size,'mtime_ns':path.stat().st_mtime_ns,'stage':c['name']}
print(json.dumps({'source':json.loads((root/'source.json').read_text()),'artifacts':artifacts,'limitation':'Hashes captured after completed stages; original execution logs retain source pin and binary paths.'},indent=2))
