from pathlib import Path
import json,re,subprocess
V=Path('/home/meszmate/.cache/pg-engine-resume-20261104/verification')
W='/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc'
head=subprocess.check_output(['git','-C',W,'rev-parse','HEAD'],text=True).strip()
assert head==(V/'source-head.txt').read_text().strip()
assert not subprocess.check_output(['git','-C',W,'status','--porcelain'],text=True).strip()
def statuses(name):
 p=V/name
 return {} if not p.exists() else {f[0]:f[1] for line in p.read_text().splitlines() if len(f:=line.split('\t'))>=2}
source=statuses('status.tsv');parity=statuses('parity-status.tsv')
logs={}
for stage in ['release','debug']:
 if source.get(stage)!='0': continue
 t=(V/(stage+'.log')).read_text()
 sums=re.findall(r'^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;',t,re.M)
 assert sums and not re.search(r'^error:|^test result: FAILED|\.\.\. FAILED',t,re.M),stage
 passed=sum(int(s[0]) for s in sums);failed=sum(int(s[1]) for s in sums);ignored=sum(int(s[2]) for s in sums)
 assert passed>=400 and failed==0,(stage,passed,failed)
 logs[stage]={'passed':passed,'failed':failed,'ignored':ignored,'summaries':len(sums)}
if source.get('clippy')=='0':
 t=(V/'clippy.log').read_text()
 assert not re.search(r'^error:',t,re.M)
 logs['clippy']={'exit':0,'warning_headings':len(re.findall(r'^warning:',t,re.M)),'zero_warning_claim':False}
if source.get('postgres-unit')=='0':
 t=(V/'postgres-unit.log').read_text()
 assert 'postgres: all green' in t or 'postgres: no changes since the last green run' in t
 logs['postgres-unit']={'exit':0,'full_service_result':False}
service=statuses('full-service/status.tsv')
result={'tested_head':head,'production_candidate':'6493cf49a1e29a1914270c530956504faa6bb0e4','source_gates':source,'parity_gates':parity,'summaries':logs,'source_complete':source.get('COMPLETE')=='0','parity_complete':parity.get('COMPLETE')=='0','full_service_complete':service.get('COMPLETE')=='0','fdw_performance':'INCONCLUSIVE: no accepted new timing','global_postgres_superiority':False}
(V/'audit-current.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result))
