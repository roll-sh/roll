"""Descriptive frequency snapshots aligned approximately with raw-log completion mtimes."""
from pathlib import Path
import json,statistics
ROOT=Path(__file__).resolve().parent
out={'limit':'Uses original cache log mtimes minus client-reported elapsed duration, not dedicated invocation timestamps. Frequency snapshots are not actual cycle counts and cannot correct or rescue a result. No causal attribution.','phases':{}}
for phase in ['proof-primary-p1','control-zero']:
 r=ROOT/phase;obs=[json.loads(l) for l in (r/'host-cpu-observation.jsonl').read_text().splitlines()]
 cases={}
 rounds=12 if phase=='proof-primary-p1' else 5
 for case,n in [('range_scan_100',8),('point_select',1)]:
  pairs=[]
  for i in range(1,rounds+1):
   arms={}
   for arm in ['baseline','candidate']:
    p=r/'candidate-vs-baseline'/f'{i:02}-{arm}-{case}-{n}.log'
    v=dict(x.split('=',1) for x in next(l for l in p.read_text().splitlines() if l.startswith('RESULT\t')).split('\t')[4:])
    end=p.stat().st_mtime;start=end-float(v['elapsed_s'])
    points=[o for o in obs if start+1<o['at_unix']<end-1]
    if not points:raise RuntimeError('no frequency observations')
    arms[arm]={'mean_reported_server_khz':statistics.mean(int(o['freq_khz'][str(c)]) for o in points for c in range(28,32)),'observations':len(points),'tps':float(v['tps'])}
   pairs.append({'round':i,**arms,'candidate_frequency_ratio':arms['candidate']['mean_reported_server_khz']/arms['baseline']['mean_reported_server_khz']})
  cases[case]={'median_frequency_ratio':statistics.median(p['candidate_frequency_ratio'] for p in pairs),'pairs':pairs}
 out['phases'][phase]=cases
(ROOT/'frequency-observation.json').write_text(json.dumps(out,indent=2)+'\n')
for phase,cases in out['phases'].items():
 print(phase,{case:round(d['median_frequency_ratio'],5) for case,d in cases.items()})
