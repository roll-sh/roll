from pathlib import Path
import hashlib,importlib.util,json,math,sys
sys.dont_write_bytecode=True
r=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('paired','crates/pg-engine/tools/bench/paired_oltp.py');paired=importlib.util.module_from_spec(spec);spec.loader.exec_module(paired)
reports={}
for directory in sorted(r.iterdir()):
 if not directory.is_dir() or not (directory/'summary.json').exists():continue
 config=json.loads((directory/'config.json').read_text());summary=json.loads((directory/'summary.json').read_text());bench=Path(config['bench']);assert hashlib.sha256(bench.read_bytes()).hexdigest()==config['bench_sha256']
 total=0;checked=0
 for target in config['targets']:
  for stage in ['before','after']:assert (directory/f"fixture-{stage}-{target['name']}.txt").read_text().strip()==config['expected_fixture']
  for case,counts in config['cases'].items():
   for clients in counts:
    assert summary[case][str(clients)][target['name']]['samples']==5
    for round_no in range(6):
     stem=directory/f"{round_no:02d}-{target['name']}-{case}-{clients}"
     values=paired.parse(stem.with_suffix('.log').read_text(),target['name'],case,clients)
     record=json.loads(stem.with_suffix('.json').read_text());assert record['exit']==0
     assert all(math.isfinite(record[key]) and record[key]>0 for key in ['server_cpu_s','client_cpu_s'])
     total+=1;checked+=int(values['checked_ops'])
 reports[directory.name]={'invocations':total,'checked_operations_including_warmup':checked,'client_sha256':config['bench_sha256'],'source_at_start':config['source_commit']}
for name in ['live-negative-results.json','final-live-negative-results.json']:
 records=json.loads((r/name).read_text());assert len(records)==8 and all(x['exit']!=0 and x['rejected'] for x in records)
(r/'completed-evidence-audit.json').write_text(json.dumps(reports,indent=2)+'\n')
print(json.dumps(reports,indent=2))
