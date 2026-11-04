from pathlib import Path
import json
import subprocess

root=Path(__file__).resolve().parent
source=(root/'check-parity.sh').read_text()
phase=source[source.index('phase() {'):source.index('\ndt() {')]
results=[]
for name,body,expected in [('early-failure','false; echo MASKED',1),('pipeline-failure','false | true; echo MASKED',1),('success','true; echo SUCCESS',0)]:
 d=root/'phase-controls'/name;d.mkdir(parents=True,exist_ok=True)
 script='set -euo pipefail\nV='+str(d)+'\n'+phase+'\nfixture() { '+body+'; }\nphase fixture fixture\n'
 r=subprocess.run(['bash','-c',script],capture_output=True,text=True)
 log=(d/'fixture.log').read_text()
 assert r.returncode==expected,(name,r.stdout,r.stderr,log)
 assert 'MASKED' not in log
 assert ('SUCCESS' in log)==(expected==0)
 results.append({'name':name,'expected_exit':expected,'actual_exit':r.returncode})
print(json.dumps({'status':'PASS','cases':results},indent=2))
