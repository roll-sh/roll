"""Fresh source gates for the frozen candidate; preserves every exit, including failures."""
from pathlib import Path
import hashlib
import json
import os
import subprocess
import time

ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')


def verify():
    frame = json.loads((ROOT / 'proof-frame.json').read_text())
    for path, digest in frame['files_sha256'].items():
        if hashlib.file_digest(Path(path).open('rb'), 'sha256').hexdigest() != digest:
            raise RuntimeError('Frozen input changed: ' + path)


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


verify()
if json.loads((ROOT / 'performance-decision.json').read_text())['action'] != 'source_checks_pending':
    raise RuntimeError('Performance rule did not pass; do not run acceptance gates')
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
    ('regression', ['python3', '-I', str(ROOT / 'run_regress.py')], 12000),
]
results = []
for name, command, seconds in checks:
    verify()
    start = time.time()
    with (out / (name + '.log')).open('x') as log:
        process = subprocess.run([str(REPO / 'crates/pg-engine/tools/heavy.sh'),
                                  'timeout', '--kill-after=180', str(seconds)] + command,
                                 cwd=REPO, stdout=log, stderr=subprocess.STDOUT, env=environment)
    result = {'name': name, 'command': command, 'exit': process.returncode,
              'started_unix': start, 'ended_unix': time.time()}
    results.append(result)
    save(out / (name + '.json'), result)
    save(out / 'status.json', results)
verify()
save(out / 'complete.json', {'all_checks_ran': True,
                             'all_exit_zero': all(r['exit'] == 0 for r in results),
                             'limit': 'Regression compares a fresh candidate to retained baseline results. PostgreSQL service failures cannot be counted as passes.'})
