"""Read-only terminal audit; keep failures and partial maps distinct from frozen full gates."""
from pathlib import Path
import json,hashlib,re,os,datetime,subprocess,collections
D=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner');E=D/'full171-lazy-v1';W='/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536';plan=json.loads((D/'full171-plan.json').read_text())
live=[]
for p in Path('/proc').iterdir():
 if not p.name.isdigit():continue
 try:
  t=(p/'stat').read_text().rsplit(')',1)[1].split()
  if int(t[2])==1841329:live.append(int(p.name))
 except(OSError,ValueError,IndexError):pass
assert not live,('Not terminal;ownedgroupstilllive',live)
wrapper=(D/'full171-lazy-v1-wrapper.log').read_text();m=re.search(r'^HEAVY-EXIT (\d+)$',wrapper,re.M);assert m,'No terminal wrapper record'
def git(*a):return subprocess.check_output(['git','-C',W,*a]).decode().strip()
problems=[];source=git('rev-parse','HEAD');tree=git('rev-parse','HEAD^{tree}');clean=not git('status','--porcelain')
if source!=plan['source']or tree!=plan['tree']or not clean:problems.append('source/tree/clean mismatch')
frozen=[]
for line in(D/'full171-frozen.sha256').read_text().splitlines():
 h,p=line.split('  ',1)
 if hashlib.sha256(Path(p).read_bytes()).hexdigest()!=h:frozen.append(p)
if frozen:problems.append('frozeninputs changed')
def ledger(path):
 rows={}
 if path.exists():
  for l in path.read_text().splitlines():
   n,s=l.split('\t');assert n not in rows,(n,path);rows[n]=int(s)
 return rows
exits=ledger(E/'exits.tsv');audit_exits=ledger(E/'audit-exits.tsv');stages={};expected=json.loads((D/'full171-expected-per-binary.json').read_text())
for cmd in plan['commands']:
 n=cmd['stage']
 if n not in exits:
  stages[n]=dict(status='NOTRUN after fail-fast stop',command_exit=None);continue
 s=(E/(n+'.log')).read_text();ok=exits[n]==0 and audit_exits.get(n+'-audit')==0
 r=dict(status='PASS'if ok else'FAILED',command_exit=exits[n],stage_audit_exit=audit_exits.get(n+'-audit'))
 if n in('debug-test','release-test'):
  key=None;actual={}
  for l in s.splitlines():
   l=l.strip()
   if l.startswith('Running '):
    z=re.match(r'Running (?:unittests )?((?:src|tests)/\S+\.rs) ',l);key=z[1]if z else None
   elif l.startswith('Doc-tests '):key='doc-tests '+l.split(None,1)[1]
   z=re.search(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored',l)
   if z and key:
    assert key not in actual,(n,key);actual[key]=list(map(int,z.groups()[1:]));key=None
  summary=dict(passed=sum(v[0]for v in actual.values()),failed=sum(v[1]for v in actual.values()),ignored=sum(v[2]for v in actual.values()),summaries=len(actual))
  full=actual==expected
  r.update(summary=summary,full_per_binary_map_matches=full)
  if ok:
   ok=full;manifest=E/(n+'-binaries.json');binaries=json.loads(manifest.read_text())if manifest.exists()else{};bad=[]
   for path,b in binaries.items():
    p=Path(b['path'])
    if not p.is_file()or hashlib.sha256(p.read_bytes()).hexdigest()!=b['sha256']:bad.append(path)
   r.update(binary_count=len(binaries),binary_mismatches=bad);ok=ok and len(binaries)==60 and not bad
  else:
   partial=dict(partial_only=True,summary=summary,per_binary=actual)
   (E/(n+'-partial-summary.json')).write_text(json.dumps(partial,indent=2)+'\n')
 if n=='clippy'and ok:
  c=json.loads((E/'clippy-warning-audit.json').read_text());ok=c['same_message_path_multiset_as_completed_c861']and c['diagnostic_count']==14;r['warning_audit']=c
 if n=='build'and ok:
  a=json.loads((E/'build-artifact.json').read_text());p=Path(a['binary']);ok=a['source']==source and a['profile']=='dist'and p.is_file()and hashlib.sha256(p.read_bytes()).hexdigest()==a['sha256'];r['artifact']=a
 if n=='postgres-unit'and ok:
  u=json.loads((E/'postgres-unit-status.json').read_text());ok=u['status']in('PASS','SKIP')and not u['service_stack_acceptance']and not u['pg_engine_execution'];r['unit_classification']=u
 if re.search(r'^error:|test result: FAILED|panicked at',s,re.M):ok=False
 floor=E/('disk-before-'+n+'.json');disk=json.loads(floor.read_text())if floor.exists()else{};r['disk_before']=disk;ok=ok and disk.get('pass_floor',False)
 r['status']='PASS'if ok else'FAILED';stages[n]=r
 if not ok:problems.append(n)
allpass=not problems and int(m[1])==0 and set(exits)=={c['stage']for c in plan['commands']}and all(x['status']=='PASS'for x in stages.values())
slots=[]
for p in Path('/tmp').glob('pg-engine-heavy.lock*/pid'):
 try:slots.append(int(p.read_text().strip()))
 except(OSError,ValueError):pass
if 1841334 in slots:allpass=False;problems.append('own heavy slot still held')
r=dict(observed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),status='FULL STANDALONE PASS'if allpass else'FAILED/PARTIAL',source=source,tree=tree,clean=clean,wrapper_exit=int(m[1]),command_exits=exits,audit_exits=audit_exits,stages=stages,problems=problems,frozen_inputs_match=not frozen,owned_process_group_absent=True,own_heavy_slot_released=1841334 not in slots,source_integration_acceptance=False,performance_acceptance=False,service_acceptance=False,serial_bound='Priorcauseunattributed;originalfailurepreserved;fullresultdoesnotestablishcausality',controls='Onlystartupandrecordedsamples;wrapper/heavyhousekeepingunrestricted;no lifetimeisolation/performanceclaim',configuration='ordinaryunfiltereddefaults only;no separatelyconfiguredstress/diagnostic/seed14/probe')
(E/'TERMINAL.json').write_text(json.dumps(r,indent=2)+'\n');print(json.dumps(dict(status=r['status'],source=source,exits=exits,stages={k:v['status']for k,v in stages.items()},problems=problems),indent=2))
