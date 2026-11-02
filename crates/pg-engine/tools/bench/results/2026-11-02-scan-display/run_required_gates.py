"""Run the promised source checks only after the locked P1 performance rule passes."""
from pathlib import Path
import importlib.util
import json
import os
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')
sys.dont_write_bytecode = True


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


os.chdir(REPO)
statistics = load('p1_statistics').evaluate(ROOT / 'paired-p1')
accounting = load('audit_pairs').audit(ROOT / 'paired-p1')
save(ROOT / 'p1-statistics.json', statistics)
save(ROOT / 'p1-accounting.json', accounting)
if not statistics['p1_statistical_rule_passes']:
    save(ROOT / 'source-gates-skipped.json', {'reason': 'Locked P1 performance rule failed; revert candidate.',
                                           'source_checks_executed': False})
    sys.exit(0)

out = ROOT / 'source-gates'
out.mkdir(exist_ok=False)
environment = dict(os.environ, CARGO_PROFILE_DEV_DEBUG='line-tables-only',
                   CARGO_PROFILE_TEST_DEBUG='line-tables-only',
                   ROLL_E2E_BIND='0.0.0.0', ROLL_E2E_THREADS='2')
checks = [
    ('build', ['cargo', 'build'], 3600),
    ('test', ['cargo', 'test'], 10800),
    ('clippy', ['cargo', 'clippy'], 3600),
    ('postgres', ['pnpm', 'test:pg', '--force'], 7200),
]
results = []
for name, command, seconds in checks:
    start = time.time()
    with (out / (name + '.log')).open('x') as log:
        process = subprocess.run(['timeout', '--kill-after=30', str(seconds)] + command,
                                 stdout=log, stderr=subprocess.STDOUT, env=environment)
    result = {'name': name, 'command': command, 'exit': process.returncode,
              'started_unix': start, 'ended_unix': time.time()}
    results.append(result)
    save(out / (name + '.json'), result)
    save(out / 'status.json', results)
save(out / 'complete.json', {'all_checks_ran': True,
                             'all_exit_zero': all(r['exit'] == 0 for r in results)})
