from pathlib import Path
import hashlib,importlib.util,json,statistics,sys
sys.dont_write_bytecode=True
r=Path(__file__).resolve().parent;directory=r/(sys.argv[1] if len(sys.argv)>1 else 'candidate-vs-baseline')
spec=importlib.util.spec_from_file_location('paired','crates/pg-engine/tools/bench/paired_oltp.py');paired=importlib.util.module_from_spec(spec);spec.loader.exec_module(paired)
config=json.loads((directory/'config.json').read_text());expected=json.loads((r/'independent-fixture.json').read_text())['expected']
assert config['expected_fixture']==expected
assert hashlib.sha256(Path(config['bench']).read_bytes()).hexdigest()==config['bench_sha256']
assert hashlib.sha256(Path('crates/pg-engine/tools/bench/paired_oltp.py').read_bytes()).hexdigest()==config['runner_sha256']
assert (r/'baseline-plans.log').read_bytes()==(r/'candidate-plans.log').read_bytes()
runtime=json.loads((r/config['runtime_provenance']).read_text())
for name,info in runtime['engines'].items():assert hashlib.sha256(Path(info['exe']).read_bytes()).hexdigest()==info['sha256']
probe=json.loads((r/'plan-choice-results.json').read_text())
for item in probe['results']:
 for i,pair in enumerate(item['counters']):
  expected_pair=([i,0] if item['mode']=='force_generic_plan' else [0,i] if item['mode']=='force_custom_plan' or item['case']=='range_scan_100' else [max(0,i-5),min(5,i)])
  assert pair==expected_pair,(item,i,pair,expected_pair)
saved={};all_samples={};total=0
for target in config['targets']:
 for stage in ['before','after']:assert (directory/f"fixture-{stage}-{target['name']}.txt").read_text().strip()==expected
 for case,counts in config['cases'].items():
  for clients in counts:
   samples=[]
   for round_no in range(6):
    stem=directory/f"{round_no:02d}-{target['name']}-{case}-{clients}"
    values=paired.parse(stem.with_suffix('.log').read_text(),target['name'],case,clients)
    resource=json.loads(stem.with_suffix('.json').read_text());assert resource['exit']==0
    for kind in ['server','client']:
     assert resource[kind+'_cpu_s']>0
     values[kind+'_cpu_us_per_op']=resource[kind+'_cpu_s']*1e6/int(values['ops'])
    total+=1
    if round_no:samples.append(values)
   stats={'samples':len(samples)}
   for key in ['tps','p50_ms','p95_ms','p99_ms','server_cpu_us_per_op','client_cpu_us_per_op','peak_mem_mb']:
    ns=[float(v[key]) for v in samples];stats[key]={'median':statistics.median(ns),'min':min(ns),'max':max(ns)}
   saved.setdefault(case,{}).setdefault(str(clients),{})[target['name']]=stats
   all_samples[case,clients,target['name']]=samples
assert saved==json.loads((directory/'summary.json').read_text())
result={'invocations':total,'candidate_source':config['source_commit'],'runtime':runtime,'cases':{}}
for case,clients_list in config['cases'].items():
 for clients in clients_list:
  a,b=saved[case][str(clients)]['baseline'],saved[case][str(clients)]['candidate']
  delta=b['tps']['median']-a['tps']['median'];spreads=[x['tps']['max']-x['tps']['min'] for x in [a,b]]
  record={'baseline':a,'candidate':b,'throughput_change_pct':delta/a['tps']['median']*100,'cpu_change_pct':(b['server_cpu_us_per_op']['median']/a['server_cpu_us_per_op']['median']-1)*100,'gain_exceeds_both_spreads':delta>max(spreads),'paired_throughput_changes_pct':[(float(y['tps'])/float(x['tps'])-1)*100 for x,y in zip(all_samples[case,clients,'baseline'],all_samples[case,clients,'candidate'])]}
  result['cases'][f'{case}-{clients}']=record
  print(case,clients,'TPS',a['tps'],b['tps'],'change%',round(record['throughput_change_pct'],3),'CPUus',round(a['server_cpu_us_per_op']['median'],3),round(b['server_cpu_us_per_op']['median'],3),'exceeds spreads',record['gain_exceeds_both_spreads'])
(r/('audited-'+directory.name+'.json')).write_text(json.dumps(result,indent=2)+'\n')
