#!/usr/bin/env python3
"""Exercise the exact saved service-runner guard snippets without services."""
from pathlib import Path
import json
import shlex
import subprocess

ROOT = Path(__file__).resolve().parent
GOOD = 'Running tests/e2e.rs\npostgres: all green\n'
CASES = [
    ('success', GOOD, 0, True),
    ('failed-summary', GOOD + 'test result: FAILED\n', 0, False),
    ('error-line', GOOD + 'error: synthetic control\n', 0, False),
    ('stamp-skip', GOOD + 'no changes since the last green run\n', 0, False),
    ('missing-green', 'Running tests/e2e.rs\n', 0, False),
    ('missing-e2e', 'postgres: all green\n', 0, False),
    ('failed-process', GOOD, 101, False),
]
results = []
for version in ('original', 'repaired'):
    source = (ROOT / f'{version}.sh').read_text()
    tail = source[source.index('if ((rc != 0))'):]
    network = source[source.index('if ((network_rc != 0))'):source.index("printf 'pnpm-test-pg-full\\tSTART")] if version == 'repaired' else ''
    for name, log, rc, accepted in CASES:
        folder = ROOT / version / name
        folder.mkdir(parents=True, exist_ok=True)
        (folder / 'pnpm-test-pg-full.log').write_text(log)
        (folder / 'status.tsv').unlink(missing_ok=True)
        script = f'set -euo pipefail\nV={shlex.quote(str(folder))}\nrc={rc}\n' + tail
        (folder / 'command.sh').write_text(script)
        run = subprocess.run(['bash', str(folder / 'command.sh')], capture_output=True)
        (folder / 'stdout').write_bytes(run.stdout)
        (folder / 'stderr').write_bytes(run.stderr)
        (folder / 'exit').write_text(f'{run.returncode}\n')
        marker = (folder / 'status.tsv').exists() and 'COMPLETE\t0' in (folder / 'status.tsv').read_text()
        expected = accepted or (version == 'original' and name in ('failed-summary', 'error-line', 'stamp-skip'))
        assert (run.returncode == 0) == expected and marker == expected, (version, name, run.returncode, marker)
        results.append(dict(version=version, case=name, exit=run.returncode, marker=marker))
    for rc in (0, 23):
        folder = ROOT / version / f'network-{rc}'
        folder.mkdir(parents=True, exist_ok=True)
        (folder / 'network-preflight.log').write_text('synthetic network control\n')
        script = f'set -euo pipefail\nV={shlex.quote(str(folder))}\nnetwork_rc={rc}\n' + network + "printf 'SERVICE_START\\n'\n"
        (folder / 'command.sh').write_text(script)
        run = subprocess.run(['bash', str(folder / 'command.sh')], capture_output=True)
        (folder / 'stdout').write_bytes(run.stdout)
        (folder / 'stderr').write_bytes(run.stderr)
        (folder / 'exit').write_text(f'{run.returncode}\n')
        expected = version == 'original' or rc == 0
        assert (run.returncode == 0) == expected and (b'SERVICE_START' in run.stdout) == expected
        if not expected:
            assert run.returncode == rc
        results.append(dict(version=version, case=f'network-{rc}', exit=run.returncode, service_start=b'SERVICE_START' in run.stdout))
(ROOT / 'results.json').write_text(json.dumps(dict(status='PASS', controls=results, services_started=False), indent=2) + '\n')
print(f'PASS {len(results)} controls; no service executed')
