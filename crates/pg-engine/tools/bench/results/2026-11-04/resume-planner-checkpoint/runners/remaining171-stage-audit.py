"""Audit only the separate first-run dist/unit stages; original failure is unchanged."""
from pathlib import Path
import json,re,sys
D=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner');E=D/'remaining171-lazy-v1';stage=sys.argv[1];log=(E/(stage+'.log')).read_text()
assert not re.search(r'^error:|test result: FAILED|panicked at',log,re.M),stage
if stage=='build':
 assert re.search(r'Compiling pg-engine v[^\n]+agent-ad5ff4bf47e789536/crates/pg-engine',log),'intended crate not freshly compiled in dist profile'
 assert re.search(r'Finished `dist` profile',log),log[-1000:]
elif stage=='postgres-unit':
 skip='postgres: no changes since the last green run; skipping'in log;executed='postgres: all green'in log
 assert skip != executed
 summaries=[list(map(int,m))for m in re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;',log)]
 if skip:assert not summaries
 else:assert len(summaries)==5 and[sum(x[i]for x in summaries)for i in range(3)]==[24,0,0],summaries
 r=dict(status='SKIP'if skip else'PASS',executed_unit_checks=not skip,summaries=len(summaries),passed=sum(x[0]for x in summaries),failed=sum(x[1]for x in summaries),ignored=sum(x[2]for x in summaries),pg_engine_execution=False,service_stack_acceptance=False,reason='stampmatched;nochecksexecuted'if skip else'--unit disablesDocker e2e;notengineorserviceacceptance')
 (E/'postgres-unit-status.json').write_text(json.dumps(r,indent=2)+'\n')
else:raise AssertionError(stage)
print(stage+'-AUDIT-PASS')
