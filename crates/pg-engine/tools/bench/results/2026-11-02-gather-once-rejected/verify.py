"""Portable read-only verification of the saved rejection, not live host state."""
import datetime,hashlib,importlib.util,json,statistics,sys
from pathlib import Path
sys.dont_write_bytecode=True
ROOT=Path(__file__).resolve().parent

def check(b,s):
 if not b:raise RuntimeError(s)
def read(p):return json.loads((ROOT/p).read_text())
def sha(p):return hashlib.file_digest(p.open('rb'),'sha256').hexdigest()
def load(name):
 s=importlib.util.spec_from_file_location(name,ROOT/(name+'.py'));m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m

def verify():
 entries={}
 for line in (ROOT/'SHA256SUMS').read_text().splitlines():
  h,name=line.split('  ',1);p=Path(name)
  check(not p.is_absolute() and '..' not in p.parts and name not in entries,'unsafe manifest entry')
  check(sha(ROOT/p)==h,'checksum '+name);entries[name]=h
 check(set(entries)=={str(p.relative_to(ROOT)) for p in ROOT.rglob('*') if p.is_file() and p.name!='SHA256SUMS'},'inventory mismatch')
 evaluator=load('evaluate');evaluator.self_test()
 primary=evaluator.evaluate(ROOT/'proof-primary-p1')
 zero=evaluator.evaluate(ROOT/'control-zero',control='zero')
 check(primary==read('proof-primary-statistics.json'),'primary mismatch')
 check(zero==read('control-zero-statistics.json'),'zero mismatch')
 check(primary['performance_rule_passes'] and not zero['performance_rule_passes'],'gate result')
 check(read('performance-decision.json')['action']=='reject','decision must reject')
 source=read('candidate-source/manifest.json');build=read('candidate-bin/manifest.json');frame=read('frame.json')
 check(len(source)==7 and source==build['source_sha256'],'seven-file build identity')
 for name,h in source.items():
  check(sha(ROOT/'candidate-source'/name)==h,'frozen source '+name)
  check(any(p.endswith('/'+name) and v==h for p,v in frame['files_sha256'].items()),'frame source '+name)
 for name in ['measure.py','evaluate.py','audit_pairs.py','run_measurements.py','run_source_gates.py','run_regress.py','HYPOTHESIS.md']:
  matches=[h for p,h in frame['files_sha256'].items() if p.endswith('/'+name)]
  check(matches==[sha(ROOT/name)],'frame tool '+name)
 check(sha(ROOT/'frame.json')==read('launch-health.json')['frame_sha256'],'frame pin')
 deps=read('matched-dependencies.json')
 check(deps['baseline']==deps['candidate'] and deps['all_fields_equal'] and len(deps['candidate']['deps'])==46,'dependency identity')
 check(deps['candidate']==read('candidate-bin/lib-pg_engine.json')==read('baseline-bin/lib-pg_engine.json'),'dependency copies')
 total=count=0
 audit=load('audit_pairs')
 for phase in ['proof-primary-p1','control-zero']:
  run=ROOT/phase;rt=read(phase+'/runtime.json')
  check(rt['source_sha256']==source,'runtime source')
  check(rt['engines']['candidate']['sha256']==build['binary_sha256']==frame['candidate_sha256'],'candidate identity')
  check(rt['engines']['baseline']['sha256']==frame['baseline_sha256'],'baseline identity')
  check(all(v['nice']==19 for v in rt['engines'].values()),'nice level')
  a=read(phase+'/baseline-preflight.json');b=read(phase+'/candidate-preflight.json')
  check(a['cases']==b['cases'],'plans/counters')
  check([x for x in a['settings'] if x[0]!='port']==[x for x in b['settings'] if x[0]!='port'],'settings')
  check(datetime.datetime.fromisoformat(frame['at_utc']).timestamp()<read(phase+'/started.json')['at_unix'],'frame must precede phase')
  actual=audit.audit(run);saved=read(('proof-primary' if phase=='proof-primary-p1' else phase)+'-accounting.json')
  actual.pop('run');saved.pop('run');check(actual==saved,'accounting mismatch')
  for p in (run/'candidate-vs-baseline').glob('*.log'):
   lines=[l for l in p.read_text().splitlines() if l.startswith('RESULT\t')];check(len(lines)==1,'result line')
   v=dict(x.split('=',1) for x in lines[0].split('\t')[4:])
   check(v['ops']==v['checked_ops'] and v['errors']=='0','checked work')
   total+=int(v['ops']);count+=1
 check((count,total)==(76,10654921),'trial totals')
 gather=read('control-zero/real-gather-control.json');samples=gather['samples']
 check(len(samples)==12 and sum(s['checked_queries'] for s in samples)==3314,'Gather query count')
 check(all(s['checked_rows']==4000*s['checked_queries'] for s in samples),'Gather rows')
 for field,key in [('server_cpu_s','cpu_ratios'),('elapsed_s','latency_ratios')]:
  ratios=[]
  for i in range(1,6):
   pair={s['name']:s for s in samples if s['round']==i};b,c=pair['baseline'],pair['candidate']
   ratios.append((c[field]/c['checked_queries'])/(b[field]/b['checked_queries']))
  check(ratios==gather[key] and statistics.median(ratios)<=1.05,'Gather ratio')
 def has_gather(n):return n.get('Node Type') in ['Gather','Gather Merge'] or any(has_gather(c) for c in n.get('Plans',[]))
 plans=read('control-zero/real-gather-plans.json')
 check(plans['baseline']==plans['candidate'] and has_gather(plans['baseline'][0]['Plan']),'Gather plan')
 for phase in ['monitor-correctness','monitor-diagnostic-r1']:
  check(not read(phase+'/complete.json')['benchmark_executed'],'monitor must be untimed')
  check(read(phase+'/baseline-monitor-correctness.json')==read(phase+'/candidate-monitor-correctness.json'),'monitor parity')
 restore=read('rejection-restore.json');live=read('final-live-cleanup.json')
 check(restore['baseline_sha256']==frame['baseline_sha256'] and restore['source_hashes_checked']==source,'restore identity')
 check(live['six_pids_absent'] and live['three_helpers_absent'] and live['six_ports_bind'] and len(live['removed_owned_paths'])==6,'recorded cleanup')
 restored=read('restored-build.json')
 check(restored['binary_sha256']==frame['baseline_sha256'] and restored['engine_diff']=='' and restored['build_exit']==0,'rebuilt baseline')
 aa=read('premise-aa/analysis.json')
 check(aa['invocations']==24 and aa['checked_operations']==2476096 and not aa['rejected_candidate_acceptance_evidence'],'AA scope')
 aa_total=aa_count=0
 for p in (ROOT/'premise-aa/run/candidate-vs-baseline').glob('*.log'):
  lines=[l for l in p.read_text().splitlines() if l.startswith('RESULT\t')]
  check(len(lines)==1,'AA result count')
  v=dict(x.split('=',1) for x in lines[0].split('\t')[4:])
  check(v['ops']==v['checked_ops'] and v['errors']=='0' and json.loads(p.with_suffix('.json').read_text())['exit']==0,'AA work/errors')
  aa_total+=int(v['ops']);aa_count+=1
 check((aa_count,aa_total)==(24,2476096),'AA counted totals')
 aa_runtime=read('premise-aa/run/runtime.json')
 check({v['sha256'] for v in aa_runtime['engines'].values()}=={frame['baseline_sha256']},'AA binary identity')
 for case,n in [('range_scan_100',8),('point_select',1)]:
  ratios=[]
  for i in range(1,6):
   values=[]
   for arm in ['baseline','candidate']:
    p=ROOT/'premise-aa/run/candidate-vs-baseline'/f'{i:02}-{arm}-{case}-{n}.log'
    v=dict(x.split('=',1) for x in next(l for l in p.read_text().splitlines() if l.startswith('RESULT\t')).split('\t')[4:]);values.append(float(v['tps']))
   ratios.append(values[1]/values[0])
  check(ratios==aa['ratios'][case]['paired_ratios'] and statistics.median(ratios)==aa['ratios'][case]['median'],'AA ratios')
 qualification=read('premise-aa/analysis-qualification.json')
 check(qualification['calibration_pass_claim_withdrawn'] and len(qualification['outside_individual_pairs'])==3,'AA qualification')
 check(read('profile-fallback/decode-cleanup.json')['profiler_stopped'],'recorded profiler cleanup')
 check(not (ROOT/'source-gates').exists(),'source acceptance gates must remain skipped')
 rows=(ROOT/'decisions.tsv').read_text().splitlines();check(rows[0].split('\t')==['ts','phase','decision','why','evidence','result'] and all(len(x.split('\t'))==6 for x in rows[1:]),'decision trail shape')
 return {'files':len(entries),'action':'rejected and reverted','trial_invocations':count,'trial_checked_operations':total,'actual_gather_checked_rows':13256000,'baseline_AA_checked_operations':2476096,'full_source_acceptance':False,'limit':'Checks durable records only; cannot recheck live processes or original binaries, recover omitted raw perf data, or independently prove each frozen source invariant.'}

if __name__=='__main__':print(json.dumps(verify(),indent=2))
