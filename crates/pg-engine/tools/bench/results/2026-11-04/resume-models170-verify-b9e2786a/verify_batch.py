#!/usr/bin/env python3
"""Run inside heavy.sh; pin source, preserve each exit, reject empty successful runs."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

plan = json.loads(Path(sys.argv[1]).read_text())
wt = Path(plan['worktree'])
out = Path(plan['output'])
out.mkdir(parents=True, exist_ok=True)

def git(*args):
    return subprocess.check_output(['git', '-C', str(wt), *args], text=True).strip()

def identity():
    return {'head': git('rev-parse', 'HEAD'), 'tree': git('rev-parse', 'HEAD^{tree}'),
            'status': git('status', '--porcelain=v1'),
            'patch_sha256': hashlib.sha256(git('diff', '--binary', 'HEAD').encode()).hexdigest()}

pinned = identity()
assert pinned['head'] == plan['head'], (pinned, plan['head'])
assert not pinned['status'], pinned
(out / 'source.json').write_text(json.dumps(pinned, indent=2))
env = dict(os.environ, **plan.get('env', {}))
env['PATH'] = '/home/meszmate/.cargo/bin:' + env['PATH']
checks = []
for stage in plan['stages']:
    if identity() != pinned:
        raise RuntimeError('Source changed before stage ' + stage['name'])
    started = time.time()
    log = out / (stage['name'] + '.log')
    with log.open('w') as stream:
        proc = subprocess.run(stage['command'], cwd=wt, env=dict(env, **stage.get('env', {})),
                              stdout=stream, stderr=subprocess.STDOUT)
    text = log.read_text(errors='replace')
    summaries = re.findall(r'^test result: .+$', text, re.M)
    passed_tests = sum(int(n) for n in re.findall(r'^test result: ok\. (\d+) passed;', text, re.M))
    fatal = re.findall(r'^(?:error(?:\[[^]]+\])?:.*|test result: FAILED.*|failures:.*)$', text, re.M)
    expected = bool(re.search(stage['expect'], text, re.M))
    check = {'name': stage['name'], 'command': stage['command'], 'env': stage.get('env', {}),
             'source_before': pinned, 'source_after': identity(), 'exit': proc.returncode,
             'expected_summary': expected, 'test_summaries': summaries, 'fatal_lines': fatal,
             'elapsed_s': time.time() - started, 'log_sha256': hashlib.sha256(log.read_bytes()).hexdigest()}
    allowed = stage.get('allows_recorded_goldens', False)
    tests_ran = 'test' not in stage['command'] or stage['command'][0] != 'cargo' or passed_tests > 0
    check['passed_tests'] = passed_tests
    check['pass'] = proc.returncode == 0 and expected and tests_ran and not fatal and (allowed or check['source_after'] == pinned)
    checks.append(check)
    (out / 'checks.json').write_text(json.dumps(checks, indent=2))
    print(json.dumps({'name': check['name'], 'exit': check['exit'], 'expected_summary': expected,
                      'pass': check['pass']}), flush=True)
    if not check['pass']:
        sys.exit(proc.returncode or 1)
print('BATCH_CHECKS_PASSED', flush=True)
