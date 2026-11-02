"""Read-only calculation of a baseline A/A diagnostic; no candidate acceptance."""
from pathlib import Path
import hashlib,json,statistics,sys
sys.dont_write_bytecode=True
r=Path(__file__).resolve().parent;run=r/'run'
def check(b,s):
 if not b:raise RuntimeError(s)
def read(p):return json.loads(p.read_text())
frame=read(r/'frame.json')
for p,h in frame['frozen_inputs'].items():check(hashlib.file_digest(Path(p).open('rb'),'sha256').hexdigest()==h,'frozen input changed')
cleanup=read(run/'cleanup.json');check(len(cleanup)==3,'cleanup count')
check(all(row.get('exit')==0 and row.get('stopped') for row in cleanup if 'pid' in row),'engine cleanup')
check(all(row.get('remove_exit')==0 for row in cleanup if 'helper_id' in row),'helper cleanup')
check(read(run/'complete.json')['source_fixture_unchanged'],'fixture')
rt=read(run/'runtime.json');check(len({x['sha256'] for x in rt['engines'].values()})==1,'not AA')
result={'kind':'baseline AA diagnostic only','rejected_candidate_acceptance_evidence':False,'ratios':{}}
total=count=0
for p in (run/'candidate-vs-baseline').glob('*.log'):
 lines=[l for l in p.read_text().splitlines() if l.startswith('RESULT\t')];check(len(lines)==1,'result')
 v=dict(x.split('=',1) for x in lines[0].split('\t')[4:]);check(v['errors']=='0' and v['ops']==v['checked_ops'],'errors')
 total+=int(v['ops']);count+=1
for case,n in [('range_scan_100',8),('point_select',1)]:
 ratios=[]
 for i in range(1,6):
  vals=[]
  for arm in ['baseline','candidate']:
   p=run/'candidate-vs-baseline'/f'{i:02}-{arm}-{case}-{n}.log'
   line=next(l for l in p.read_text().splitlines() if l.startswith('RESULT\t'))
   vals.append(float(dict(x.split('=',1) for x in line.split('\t')[4:])['tps']))
  ratios.append(vals[1]/vals[0])
 result['ratios'][case]={'paired_ratios':ratios,'median':statistics.median(ratios),'outside_three_percent_band':not(.97<=statistics.median(ratios)<=1/.97)}
result['checked_operations']=total;result['invocations']=count
result['comparison_instability_observed']=any(x['outside_three_percent_band'] for x in result['ratios'].values())
print(json.dumps(result,indent=2))
