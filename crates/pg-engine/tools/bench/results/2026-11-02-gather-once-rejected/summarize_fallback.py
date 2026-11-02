"""Summarize original-map sample weights without converting them to time per op."""
from pathlib import Path
from collections import Counter
import json
R=Path('/home/meszmate/.cache/pg-engine-perf-20261101/gather-fallback-20261102')
needles={'gathers':'pg_engine::sql::explain::gathers','serial_describe':'pg_engine::sql::paths::describe_need','relation_model':'<pg_engine::sql::paths::Planner>::rel','partial_paths':'pg_engine::sql::paths::parallel::partial_scans','builder_scan':'<pg_engine::sql::explain::Builder>::scan','endpoint_reads':'pg_engine::exec::scan::column_range','probe':'pg_engine::sql::explain::gather_proof'}
result={'limits':'Inclusive ancestry overlaps. Finite snapshots and inlining obscure frames. Fractions are diagnostic weights, not removable CPU or phase time per operation. Whole-client CPU and sample-window work differ. No accepted gain.','workloads':{}}
for case in ['range_scan_100','point_select']:
 out={}
 for arm in ['baseline','candidate']:
  samples=[s for p in (R/arm).glob('*'+case+'*-classified.json') for s in json.loads(p.read_text())['samples']]
  leaves=Counter();weights=Counter();total=sum(s['period'] for s in samples)
  for s in samples:
   names=[f['symbol'] or '' for f in s['frames']]
   leaves[names[0] or '[unresolved]']+=s['period']
   for k,n in needles.items():
    if any(n in name for name in names):weights[k]+=s['period']
  out[arm]={'samples':len(samples),'total_period':total,'inclusive_fraction':{k:weights[k]/total for k in needles},'top_leaf_fraction':{k:v/total for k,v in leaves.most_common(20)}}
 result['workloads'][case]=out
(R/'summary.json').write_text(json.dumps(result,indent=2)+'\n')
for case,arms in result['workloads'].items():
 print(case)
 for arm,v in arms.items():print(arm,v['samples'],{k:round(100*x,2) for k,x in v['inclusive_fraction'].items()},list(v['top_leaf_fraction'].items())[:8])
