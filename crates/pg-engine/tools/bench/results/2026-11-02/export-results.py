from pathlib import Path
import csv,hashlib,json,shutil,subprocess
r=Path(__file__).resolve().parent
out=Path('crates/pg-engine/tools/bench/results/2026-11-02');out.mkdir(parents=True,exist_ok=True)
comparisons={}
headers=['comparison','round','target','case','clients','tps','ops','elapsed_s','checked_ops','validation_version','errors','p50_ms','p95_ms','p99_ms','peak_mem_mb','server_cpu_s','client_cpu_s','server_cpu_us_per_op','client_cpu_us_per_op']
with (out/'oltp.tsv').open('w') as f:
 writer=csv.DictWriter(f,fieldnames=headers,delimiter='\t',lineterminator='\n');writer.writeheader()
 for name in ['checked-baseline-vs-pg','borrows-vs-baseline','histogram-vs-borrows','histogram-vs-baseline','final-vs-pg','final-dirty-vs-baseline','point-control-vs-baseline','restarted-dirty-vs-baseline']:
  directory=r/name
  if not (directory/'summary.json').exists():continue
  config=json.loads((directory/'config.json').read_text());summary=json.loads((directory/'summary.json').read_text());comparisons[name]={'config':config,'summary':summary}
  raw=out/'raw'/name;raw.mkdir(parents=True,exist_ok=True)
  for artifact in directory.iterdir():
   if artifact.is_file() and artifact.suffix in ['.log','.json','.txt']:shutil.copy2(artifact,raw/artifact.name)
  observed=directory/'runtime-observation.json'
  if not observed.exists() and config.get('runtime_provenance') != 'runtime-latest.json':
   observed=r/config.get('runtime_provenance','missing-runtime.json')
  if observed.exists():
   comparisons[name]['runtime_observation']=json.loads(observed.read_text())
   comparisons[name]['runtime_observation_sha256']=hashlib.sha256(observed.read_bytes()).hexdigest()
  for log in sorted(directory.glob('*.log')):
   lines=[line for line in log.read_text().splitlines() if line.startswith('RESULT\t')]
   if not lines:continue
   assert len(lines)==1
   parts=lines[0].split('\t');values=dict(part.split('=',1) for part in parts[4:]);ops=int(values['ops']);assert values['errors']=='0' and int(values['checked_ops'])==ops
   resource=json.loads(log.with_suffix('.json').read_text());assert resource['exit']==0
   row={'comparison':name,'round':int(log.name.split('-')[0]),'target':parts[1],'case':parts[3]}
   row.update({key:values[key] for key in headers if key in values})
   for kind in ['server','client']:
    cpu=resource[kind+'_cpu_s'];row[kind+'_cpu_s']=cpu;row[kind+'_cpu_us_per_op']=cpu*1e6/ops
   writer.writerow(row)
manifest={'export_source':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'evidence_root':str(r),'comparisons':comparisons}
for name in ['manifest.json','independent-fixture.json','plan-choice-results.json','live-negative-results.json','final-live-negative-results.json','completed-evidence-audit.json','final-verification.json','performance-cleanup.json','final-cleanup.json','runtime-latest.json']:
 if (r/name).exists():shutil.copy2(r/name,out/name)
for name in ['baseline-settings.log','postgres-settings.log','baseline-plans.log','postgres-plans.log','probe-plan-choice.py','independent-fixture.py','check-live-negative.py','check-final-live-negative.py','run-final-gates.sh','run-final-regress.sh','histogram-build-tests.log','final-client-build-tests.log','audit-final.py','audit-results.py','export-results.py','NEXT-OLTP.md']:
 if (r/name).exists():shutil.copy2(r/name,out/name)
for name in ['checked-client','final-client','borrows-bin','histogram-bin']:
 if (r/name/'manifest.json').exists():shutil.copy2(r/name/'manifest.json',out/(name+'-manifest.json'))
shutil.copy2(r.parent/'inline-bin/build.json',out/'baseline-engine-build.json')
(out/'comparisons.json').write_text(json.dumps(manifest,indent=2)+'\n')
for pattern in ['negative-*-*.log','final-negative-*-*.log']:
 for log in r.glob(pattern):shutil.copy2(log,out/log.name)
shutil.copy2('crates/pg-engine/tools/bench/paired_oltp.py',out/'paired_oltp.py')
shutil.copytree(r/'final-gates',out/'final-gates',dirs_exist_ok=True)
for label,root in [('baseline',r.parent/'final-regress-7f35a185-release'),('current',r/'final-regress-5b32046d-release')]:
 destination=out/'regression'/label;destination.mkdir(parents=True,exist_ok=True)
 shutil.copytree(root/'report',destination/'report',dirs_exist_ok=True)
 for name in ['status.tsv','commit.txt','engine.sha256','reference-version.txt','regress.log','cleanup.log']:
  if (root/name).exists():shutil.copy2(root/name,destination/name)
for name in ['runtime-2026-11-02T000407.679797_0000.json','runtime-2026-11-02T001318.177670_0000.json','runtime-2026-11-02T003823.959454_0000.json','runtime-before-memory-control.json']:
 shutil.copy2(r/name,out/name)
print(f'Exported {len(comparisons)} comparisons to {out}')
