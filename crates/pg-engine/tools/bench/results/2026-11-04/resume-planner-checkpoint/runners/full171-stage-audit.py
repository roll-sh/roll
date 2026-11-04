"""Audit a single completed stage without rerunning commands or altering frozen gates."""
from pathlib import Path
import collections,hashlib,json,re,sys,subprocess
D=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner');E=D/'full171-lazy-v1';stage=sys.argv[1];log=(E/(stage+'.log')).read_text()
if stage in('debug-test','release-test'):
 subprocess.run([sys.executable,str(D/'audit165-summary.py'),str(E/(stage+'.log')),str(D/'full171-expected-per-binary.json'),str(E/(stage+'-summary.json'))],check=True)
 # Record the actual executable names and content identities while this profile is pinned.
 binaries={}
 for line in log.splitlines():
  m=re.search(r'Running (?:unittests )?((?:src|tests)/\S+\.rs) \(([^\n]+)\)',line)
  if m:
   p=Path(m[2]);binaries[m[1]]=dict(path=str(p),sha256=hashlib.sha256(p.read_bytes()).hexdigest(),mtime_ns=p.stat().st_mtime_ns)
 assert len(binaries)==60,len(binaries)
 (E/(stage+'-binaries.json')).write_text(json.dumps(binaries,indent=2)+'\n')
elif stage=='clippy':
 rows=[]
 for line in log.splitlines():
  try:rows.append(json.loads(line))
  except json.JSONDecodeError:pass
 assert any(x.get('reason')=='build-finished' and x.get('success')for x in rows)
 msgs=[x['message']for x in rows if x.get('reason')=='compiler-message'];assert not[x for x in msgs if x['level']=='error']
 warnings=[dict(message=x['message'],path=s['file_name'])for x in msgs if x['level']=='warning'for s in x.get('spans',[])if s.get('is_primary')]
 old=json.loads((D/'check165ind-v3/clippy-warning-audit.json').read_text())['diagnostics']
 got=collections.Counter((x['message'],x['path'])for x in warnings);want=collections.Counter((x['message'],x['path'])for x in old)
 report=dict(command_exit=0,diagnostic_count=len(warnings),diagnostics=warnings,same_message_path_multiset_as_completed_c861=got==want,comparison_order_ignored=True,extra=list((got-want).elements()),missing=list((want-got).elements()),reference='completedc861 releaseall-target14warnings;no newbaselinerun')
 (E/'clippy-warning-audit.json').write_text(json.dumps(report,indent=2)+'\n');assert got==want,report
elif stage=='build':
 assert re.search(r'Finished `dist` profile',log),log[-1000:]
elif stage=='postgres-unit':
 skip='postgres: no changes since the last green run; skipping'in log;allgreen='postgres: all green'in log
 assert skip != allgreen
 summaries=[list(map(int,m))for m in re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;',log)]
 if skip:assert not summaries
 else:assert len(summaries)==5 and[sum(x[i]for x in summaries)for i in range(3)]==[24,0,0],summaries
 report=dict(status='SKIP'if skip else'PASS',executed_unit_checks=not skip,summaries=len(summaries),passed=sum(x[0]for x in summaries),failed=sum(x[1]for x in summaries),ignored=sum(x[2]for x in summaries),pg_engine_execution=False,service_stack_acceptance=False,reason='stampmatched;nochecksexecuted'if skip else'--unit disablesDocker e2e;notengineorserviceacceptance')
 (E/'postgres-unit-status.json').write_text(json.dumps(report,indent=2)+'\n')
else:raise AssertionError(stage)
assert not re.search(r'^error:|test result: FAILED|panicked at',log,re.M),stage
print(stage+'-AUDIT-PASS')
