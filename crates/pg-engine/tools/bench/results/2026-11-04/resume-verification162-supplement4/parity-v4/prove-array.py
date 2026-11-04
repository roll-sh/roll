#!/usr/bin/env python3
from pathlib import Path
import hashlib,json,subprocess,sys
v=Path(__file__).resolve().parent;old=v.parent
out=v/'array-validator-proof';out.mkdir(exist_ok=True)
real=(old/'array-current/run.log').read_text()
fixture=(old/'array-header.sql').read_text();(out/'array-header.sql').write_text(fixture)
assert 'test probe ... > ' in real
markers=[line for line in fixture.splitlines() if line.startswith(r'\x2 ')]
missing=real.replace('> '+markers[3]+'\n','',1)
duplicate=real.replace('> '+markers[3]+'\n','> '+markers[2]+'\n',1)
variants={'real-prefixed-pass':real,'injected-failure':real.replace('SELECT $1::int[]','!! wrong_error\nSELECT $1::int[]',1),'missing-case':missing,'duplicate-case':duplicate,'truncated-tail':real[:real.index('test result:')],'real-baseline-failure':(old/'array-base/run.log').read_text()}
results=[]
for name,stream in variants.items():
 p=out/(name+'.log');p.write_text(stream)
 command=[sys.executable,str(v/'audit-array.py'),str(p),str(out/'array-header.sql')]
 r=subprocess.run(command,capture_output=True,text=True)
 (out/(name+'.stdout.log')).write_text(r.stdout);(out/(name+'.stderr.log')).write_text(r.stderr)
 expected=0 if name=='real-prefixed-pass' else 1
 result={'case':name,'command':command,'exit':r.returncode,'expected_exit':expected,'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'contains_true_failure_marker':'!! wrong_error' in stream}
 results.append(result)
 assert r.returncode==expected,result
assert sum(line.startswith(r'> \x2 ') for line in real.splitlines())==7
proof={'status':'PASS','old_anchored_validator_count':7,'actual_executed_cases':8,'cases':results}
(out/'proof.json').write_text(json.dumps(proof,indent=2)+'\n');print(json.dumps(proof,indent=2))
