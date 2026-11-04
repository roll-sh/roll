"""Read-only audit for separate dist/unit completion; original full failure stays failed."""
from pathlib import Path
import collections,datetime,hashlib,json,os,re,shlex,subprocess
D=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner');E=D/'remaining171-lazy-v1';W='/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536'
launch=json.loads((D/'remaining171-launch.json').read_text());plan=json.loads((D/'remaining171-plan.json').read_text());group=launch['process_group'];live=[]
for p in Path('/proc').iterdir():
 if not p.name.isdigit():continue
 try:
  t=(p/'stat').read_text().rsplit(')',1)[1].split()
  if int(t[2])==group:live.append(int(p.name))
 except(OSError,ValueError,IndexError):pass
assert not live,('Notterminal',live)
wrapper=(D/'remaining171-lazy-v1-wrapper.log').read_text();m=re.search(r'^HEAVY-EXIT (\d+)$',wrapper,re.M);assert m,'No terminal wrapper record'
problems=[];frozen=[]
for file in('full171-frozen.sha256','remaining171-frozen.sha256'):
 for line in(D/file).read_text().splitlines():
  h,p=line.split('  ',1)
  if hashlib.sha256(Path(p).read_bytes()).hexdigest()!=h:frozen.append(p)
if frozen:problems.append('Frozeninputsmismatch')
def git(*args):return subprocess.check_output(['git','-C',W,*args]).decode().strip()
source=git('rev-parse','HEAD');tree=git('rev-parse','HEAD^{tree}');clean=not git('status','--porcelain')
if source!=plan['source']or tree!=plan['tree']or not clean:problems.append('Source/tree/cleanmismatch')
original=json.loads((D/'full171-lazy-v1/TERMINAL.json').read_text());oldpreserved=original['status']=='FAILED/PARTIAL'and original['wrapper_exit']==1 and original['audit_exits'].get('clippy-audit')==1 and original['stages']['build']['status'].startswith('NOTRUN')and original['stages']['postgres-unit']['status'].startswith('NOTRUN')
if not oldpreserved:problems.append('Originalfailurestatusmismatch')
def ledger(p):
 rows={}
 if p.exists():
  for l in p.read_text().splitlines():
   n,s=l.split('\t');assert n not in rows;rows[n]=int(s)
 return rows
exits=ledger(E/'exits.tsv');audits=ledger(E/'audit-exits.tsv');stages={}
for cmd in plan['commands']:
 n=cmd['stage']
 if n not in exits:stages[n]=dict(status='NOTRUN after fail-fast stop',command_exit=None);continue
 log=(E/(n+'.log')).read_text();ok=exits[n]==0 and audits.get(n+'-audit')==0 and shlex.split((E/(n+'.command')).read_text())==cmd['argv']
 r=dict(command_exit=exits[n],stage_audit_exit=audits.get(n+'-audit'));floor=json.loads((E/('disk-before-'+n+'.json')).read_text());r['disk_before']=floor;ok=ok and floor['pass_floor']
 if re.search(r'^error:|test result: FAILED|panicked at',log,re.M):ok=False
 if n=='build'and ok:
  ok=bool(re.search(r'Compiling pg-engine v[^\n]+agent-ad5ff4bf47e789536/crates/pg-engine',log)and re.search(r'Finished `dist` profile',log))
  a=json.loads((E/'build-artifact.json').read_text());p=Path(a['binary']);actual=hashlib.sha256(p.read_bytes()).hexdigest();target=Path(a['source_artifact'])
  ok=ok and a['source']==source and a['profile']=='dist'and actual==a['sha256']and actual==hashlib.sha256(target.read_bytes()).hexdigest();r['artifact']=a
 if n=='postgres-unit'and ok:
  u=json.loads((E/'postgres-unit-status.json').read_text());skip='postgres: no changes since the last green run; skipping'in log;executed='postgres: all green'in log;ok=(skip!=executed)and u['executed_unit_checks']==executed and not u['pg_engine_execution']and not u['service_stack_acceptance']
  summaries=[list(map(int,m))for m in re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;',log)]
  ok=ok and (not summaries if skip else len(summaries)==5 and[sum(x[i]for x in summaries)for i in range(3)]==[24,0,0]);r['unit_classification']=u
 r['status']='PASS'if ok else'FAILED';stages[n]=r
 if not ok:problems.append(n)
slots=[]
for p in Path('/tmp').glob('pg-engine-heavy.lock*/pid'):
 try:slots.append(int(p.read_text().strip()))
 except(OSError,ValueError):pass
released=3890752 not in slots
if not released:problems.append('Ownheavyslotstillheld')
allok=not problems and int(m[1])==0 and set(exits)=={'build','postgres-unit'}and all(s['status']=='PASS'for s in stages.values())
r=dict(observed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),remaining_status='SEPARATE DIST/UNIT COMPLETE'if allok else'FAILED/PARTIAL',source=source,tree=tree,clean=clean,wrapper_exit=int(m[1]),command_exits=exits,audit_exits=audits,stages=stages,problems=problems,original_pipeline_status='FAILED/PARTIAL;Clippyaudit1/HEAVY1;dist/unitNOTRUNthere',original_failure_preserved=oldpreserved,old_and_new_frozen_inputs_match=not frozen,owned_process_group_absent=True,own_heavy_slot_released=released,source_integration_acceptance=False,performance_acceptance=False,service_acceptance=False,serial_bound='OriginalcauseUNATTRIBUTED;passingchecksnotcausalproof',controls='Startupandrecordedsamplesonly;wrapper/heavyhousekeepingunrestricted',auditor_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
(E/'TERMINAL.json').write_text(json.dumps(r,indent=2)+'\n');print(json.dumps({k:r[k]for k in('remaining_status','source','command_exits','audit_exits','original_pipeline_status','problems')},indent=2))
