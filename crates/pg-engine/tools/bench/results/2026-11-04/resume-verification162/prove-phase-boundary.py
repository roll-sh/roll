#!/usr/bin/env python3
"""No-service regression proof for conditional suppression of Bash errexit."""
from pathlib import Path
import hashlib
import json
import shlex
import subprocess

root=Path(__file__).resolve().parent
out=root/'phase-boundary-proof'
out.mkdir(exist_ok=True)
implementations={
 'conditional':root/'parity-attempt1/check-parity.sh',
 'subshell':root/'check-parity.sh',
}
fixtures={
 'early-failure':("fixture() { sh -c 'exit 23'; touch \"$marker\"; }",23),
 'pipeline-failure':("fixture() { sh -c 'exit 29' | cat >/dev/null; touch \"$marker\"; }",29),
 'nested-function':("inner() { sh -c 'exit 31'; }\nfixture() { inner; touch \"$marker\"; }",31),
 'success':('fixture() { true; touch "$marker"; }',0),
}
results=[]
for implementation,path in implementations.items():
 source=path.read_text()
 phase=source[source.index('phase() {'):source.index('\ndt() {')]
 frozen=out/(implementation+'.sh');frozen.write_text(phase+'\n')
 for name,(body,failure) in fixtures.items():
  case=out/implementation/name;case.mkdir(parents=True,exist_ok=True)
  marker=case/'success-marker'
  script='set -euo pipefail\nV='+shlex.quote(str(case))+'\nmarker='+shlex.quote(str(marker))+'\nrm -f -- "$marker"\n'+phase+'\n'+body+'\nphase fixture fixture\n'
  (case/'commands.sh').write_text(script)
  run=subprocess.run(['bash',str(case/'commands.sh')],capture_output=True,text=True)
  (case/'stdout.log').write_text(run.stdout);(case/'stderr.log').write_text(run.stderr)
  expected=failure if implementation=='subshell' else 0
  expected_marker=implementation=='conditional' or failure==0
  result={'implementation':implementation,'case':name,'original_failure':failure,'expected_exit':expected,'actual_exit':run.returncode,'marker_created':marker.exists(),'expected_marker':expected_marker}
  (case/'exit.json').write_text(json.dumps(result,indent=2)+'\n')
  assert run.returncode==expected and marker.exists()==expected_marker,result
  results.append(result)
summary={'status':'PASS','scope':'no service, source suite or benchmark; exact saved phase implementations','implementations':{name:hashlib.sha256((out/(name+'.sh')).read_bytes()).hexdigest() for name in implementations},'cases':results}
(out/'proof.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps(summary,indent=2))
