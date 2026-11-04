#!/usr/bin/env python3
"""Two deterministic red checks on pinned HEAD plus a recorded test-only patch."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

root = Path('/home/meszmate/.cache/pg-engine-resume-20261104/execution_models')
wt = Path('/home/meszmate/roll/.claude/worktrees/agent-a814fa71b51112148')
out = root / 'a149-endpoint-red'
out.mkdir(exist_ok=True)
def git(*args):
    return subprocess.check_output(['git', '-C', str(wt), *args])
def identity():
    return {'head': git('rev-parse','HEAD').decode().strip(),
            'tree': git('rev-parse','HEAD^{tree}').decode().strip(),
            'status': git('status','--porcelain=v1').decode(),
            'patch_sha256': hashlib.sha256(git('diff','--binary','HEAD')).hexdigest()}
pinned = identity()
assert pinned['head'] == '2a854a48074dbd3317807f634a773f29540b2673'
patch = git('diff','--binary','HEAD')
(out / 'test-only.patch').write_bytes(patch)
(out / 'source.json').write_text(json.dumps(pinned, indent=2)+'\n')
env = dict(os.environ, PATH='/home/meszmate/.cargo/bin:'+os.environ['PATH'],
           TMPDIR='/home/meszmate/.cache/a149/tmp', CARGO_TARGET_DIR='/home/meszmate/.cache/a149/target',
           CARGO_BUILD_JOBS='2', RUST_TEST_THREADS='2')
stages = [('storage', ['--lib', 'storage::tests::point_fetch_endpoint_matches_get_cold_and_hot'],
           'point-fetch endpoint must include physical MVCC versions before hot promotion'),
          ('sql', ['--test', 'app_concurrency', 'secondary_index_update_cold_and_hot'],
           'secondary UPDATE plan=')]
results = []
for name, args, marker in stages:
    assert identity() == pinned
    cmd = ['cargo','test','-p','pg-engine',*args,'--','--exact','--nocapture']
    with (out / (name+'.log')).open('w') as log:
        proc = subprocess.run(cmd, cwd=wt, env=env, stdout=log, stderr=subprocess.STDOUT)
    raw = (out / (name+'.log')).read_text(errors='replace')
    binaries = {}
    for binary in re.findall(r'Running .+ \((/[^)]+)\)', raw):
        p = Path(binary)
        binaries[binary] = {'sha256': hashlib.sha256(p.read_bytes()).hexdigest(), 'bytes': p.stat().st_size}
    expected = proc.returncode == 101 and marker in raw and 'test result: FAILED. 0 passed; 1 failed;' in raw and not re.search(r'^error\[',raw,re.M)
    row = {'stage':name,'command':cmd,'source_before':pinned,'source_after':identity(),
           'exit':proc.returncode,'expected_red':expected,'test_summaries':re.findall(r'^test result:.*$',raw,re.M),
           'binaries':binaries,'log_sha256':hashlib.sha256((out/(name+'.log')).read_bytes()).hexdigest()}
    results.append(row)
    (out/'checks.json').write_text(json.dumps(results,indent=2)+'\n')
    print(json.dumps(row), flush=True)
    if not expected or identity() != pinned:
        sys.exit(proc.returncode or 1)
print('BOTH_EXPECTED_RED_RESULTS_RECORDED; candidate correctness remains FAILED',flush=True)
