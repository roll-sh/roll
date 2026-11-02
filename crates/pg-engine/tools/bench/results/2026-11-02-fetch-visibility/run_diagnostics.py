"""Exercise every setup path without any timed client invocation, before the frame exists."""
from pathlib import Path
import json,subprocess,time
ROOT=Path(__file__).resolve().parent
REPO=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
if (ROOT/'frame.json').exists():raise RuntimeError('Cannot run draft diagnostics after freeze')
for index,(name,flags) in enumerate([('monitor-diagnostic-r1',[]),('dirty-diagnostic-r1',['--control','dirty']),('swap-diagnostic-r1',['--swap-slots'])]):
    command=[str(REPO/'crates/pg-engine/tools/heavy.sh'),'timeout','--kill-after=90','600','python3','-I',str(ROOT/'measure.py'),'--out',str(ROOT/name),'--candidate-dir',str(ROOT/'candidate-bin'),'--monitor','--preflight-only','--port0',str(53740+2*index)]+flags
    start=time.time()
    with (ROOT/(name+'-console.log')).open('x') as log:
        process=subprocess.run(command,cwd=REPO,stdout=log,stderr=subprocess.STDOUT)
    with (ROOT/(name+'-status.json')).open('x') as out:json.dump({'command':command,'exit':process.returncode,'started_unix':start,'ended_unix':time.time()},out,indent=2);out.write('\n')
    if process.returncode:raise RuntimeError(name+' failed; no timing attempted')
print('All three untimed setup diagnostics passed.')
