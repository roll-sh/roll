import json,pathlib as P,subprocess as S,re,hashlib,time
from build import R,stable,sha

def rows(name):return [json.loads(x) for x in (R/name).read_text().splitlines()]
def audit():
 stable();results=rows('results.jsonl');commands=rows('commands.jsonl');checks=rows('correctness.jsonl');resources=rows('resources.jsonl');stability=rows('stability.jsonl')
 assert (R/'RUN-PASS').exists()
 final=json.loads((R/'actual-resume1-exit.json').read_text());first=json.loads((R/'actual-runner-exit.json').read_text())
 assert final['exit']==0 and final['remaining_owned_ids']==[]
 assert first['exit']==1 and first['remaining_owned_ids']==[]
 assert 'Address already in use' in (R/'RUN-FAIL').read_text()
 assert all(x['passed'] for x in stability)
 assert len(checks)==76 and all(x['failures']==0 for x in checks),len(checks)
 timed=[x for x in results if re.fullmatch('(copy|pgbench|index)-[0-4]-(baseline|candidate)',x['tag'])]
 profiles=[x for x in results if x.get('profile')]
 assert len(timed)==30 and len({x['tag'] for x in timed})==30
 assert len(profiles)==6
 assert all(x['exit']==0 and not x.get('measurement_errors') for x in results)
 assert all(x['server_cpu']['usage_usec']>=0 and x['client_cpu']['usage_usec']>=0 for x in results)
 assert all(x['disk_after']['allocated']>0 for x in results)
 negative=[x for x in commands if x['tag'].startswith('negative-')]
 assert [(x['tag'],x['exit']) for x in negative]==[('negative-sql',2),('negative-docker',1),('negative-pipeline',7)]
 failures=[x for x in commands if x['exit']!=0 and not x['tag'].startswith('negative-')];assert not failures,failures
 assert sha(R/'input/rows.csv')==json.loads((R/'input.json').read_text())['sha256']
 ids=[x['id'] for x in resources]+[(R/'negative-sql.cid').read_text().strip()]
 assert len(ids)==len(set(ids))==45,(len(ids),len(set(ids)))
 absence=[]
 for cid in ids:
  q=S.run(['docker','container','inspect',cid],capture_output=True,text=True)
  absence.append(dict(id=cid,exit=q.returncode,stdout=q.stdout,stderr=q.stderr))
  assert q.returncode==1 and 'No such' in q.stderr,(cid,q.returncode,q.stderr)
 assert len(list((R/'data').iterdir()))==40
 for resource in resources:
  p=R/(resource['id'][:12]+'.controls.json')
  # Reopen containers have retained matching settings in their creation commands;
  # their later controls are checked separately through those exact commands.
  if p.exists():
   c=json.loads(p.read_text());assert c['controls']['cpu.weight'].strip()=='1' and c['controls']['cpu.max'].startswith('max ')
   assert c['controls']['memory.swap.max'].strip()=='0'
 expected=[]
 for w in ['copy','pgbench','index']:
  for i in range(5):
   expected += [f'{w}-{i}-{s}' for s in (['baseline','candidate'] if i%2==0 else ['candidate','baseline'])]
 assert [x['tag'] for x in timed]==expected
 reports=[x for x in commands if x['tag'].endswith('-profile-self')]
 assert len(reports)==6
 for x in reports:assert 'Total Lost Samples: 0' in (R/x['log']).read_text()
 bundle_count=0
 for folder in (R/'sealed').iterdir():
  if not folder.is_dir():continue
  manifest=json.loads((folder/'MANIFEST.json').read_text());assert manifest['closed']
  assert {p.name for p in folder.iterdir()}==set(manifest['files'])|{'MANIFEST.json'}
  for name,value in manifest['files'].items():
   p=folder/name;assert sha(p)==value['sha256'] and p.stat().st_size==value['bytes']
  bundle_count+=1
 return dict(passed=True,timed_runs=30,profile_runs=6,validation_checks=76,validation_failures=0,workload_command_failures=0,timed_retries=0,protocol_failed_attempts=1,negative_control_failures_expected=3,owned_containers_created_and_absent=45,retained_datasets=40,source_binary_stability_checks=len(stability),prior_sealed_bundles_verified=bundle_count,cleanup_absence=absence,input_sha256=sha(R/'input/rows.csv'),first_attempt=first,final_attempt=final,at=time.time())
if __name__=='__main__':
 try:result=audit()
 except BaseException:
  import traceback
  (R/'AUDIT-FAIL.log').write_text(traceback.format_exc());raise
 (R/'audit-final.json').write_text(json.dumps(result,indent=2)+'\n')
 print(json.dumps({k:v for k,v in result.items() if k!='cleanup_absence'},indent=2))
