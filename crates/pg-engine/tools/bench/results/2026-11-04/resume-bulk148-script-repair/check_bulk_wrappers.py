#!/usr/bin/env python3
"""Exercise wrapper exits with local fakes, never starting Docker or a service."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

out = Path('/home/meszmate/.cache/pg-engine-resume-20261104/execution_models/bulk148-script-repair')
out.mkdir(exist_ok=True)
source = Path('/home/meszmate/.cache/a148/bench')
cases = []
with tempfile.TemporaryDirectory(prefix='fake-wrapper-', dir=out) as tmp:
    root = Path(tmp)
    home = root / 'home'
    bench = home / '.cache/a148/bench'
    bench.mkdir(parents=True)
    (home / '.cache/a148/work').mkdir()
    binpath = root / 'bin'
    binpath.mkdir()
    cgroot = root / 'cgroup'
    cg = cgroot / 'docker-fake.scope'
    cg.mkdir(parents=True)
    (cg / 'cpu.stat').write_text('usage_usec 0\n')
    (cg / 'io.stat').write_text('8:0 wbytes=0\n')
    (cg / 'memory.peak').write_text('1048576\n')
    docker = binpath / 'docker'
    docker.write_text('''#!/usr/bin/env python3
import os, sys
a=sys.argv[1:]
if a[0]=='inspect':
    if os.environ.get('FAIL_DOCKER')=='1': sys.exit(29)
    print('fake')
elif a[0]=='exec':
    if 'psql' in a:
        sql=a[-1]
        if os.environ.get('FAIL_SQL') and os.environ['FAIL_SQL'] in sql:
            print('ERROR: deliberate fake SQL failure', file=sys.stderr); sys.exit(23)
        print('FAKE_SQL_OK')
    else: print('0')
else: sys.exit(91)
''')
    docker.chmod(0o755)
    run = root / 'fake-run.sh'
    run.write_text('''#!/bin/sh
set -eu
case "$1" in
pg-start) [ "${FAIL_RUN:-}" != 1 ] || exit 31 ;;
engine-start) mkdir -p "$DATA/engine"; echo 1 > "$DATA/engine.pid" ;;
pg-stop|engine-stop) : ;;
*) exit 92 ;;
esac
''')
    run.chmod(0o755)
    scripts = {}
    for name in ['step.sh', 'bulk.sh', 'crun.sh']:
        text = (source / name).read_text()
        # Redirect only fixture filesystem probes; timing/failure logic is the real script.
        text = text.replace('/sys/fs/cgroup/system.slice', str(cgroot))
        if name == 'crun.sh':
            old = 'R=/home/meszmate/roll/.claude/worktrees/agent-ad1ff70de0f5338c1/crates/pg-engine/tools/bench/run.sh'
            assert old in text
            text = text.replace(old, 'R=' + str(run))
        p = bench / name
        p.write_text(text)
        p.chmod(0o755)
        scripts[name] = p
        (out / ('fixture-' + name)).write_text(text)
    (out / 'fake-docker.py').write_bytes(docker.read_bytes())
    (out / 'fake-run.sh').write_bytes(run.read_bytes())
    env = dict(os.environ, HOME=str(home), PATH=str(binpath) + ':' + os.environ['PATH'])
    def check(name, command, expected, extra=None, contains=None):
        proc = subprocess.run(command, env=dict(env, **(extra or {})), text=True, capture_output=True)
        (out / (name + '.stdout')).write_text(proc.stdout)
        (out / (name + '.stderr')).write_text(proc.stderr)
        result = {'name': name, 'exit': proc.returncode, 'expected_exit': expected,
                  'pass': proc.returncode == expected and (contains is None or contains in proc.stdout)}
        cases.append(result)
        print(json.dumps(result), flush=True)
        assert result['pass'], proc
    check('step-success', ['sh', str(scripts['step.sh']), 'pg', 'select', 'bench', 'SELECT 1'], 0, contains='wall ')
    check('step-sql-failure', ['sh', str(scripts['step.sh']), 'pg', 'select', 'bench', 'SELECT 1'], 23, {'FAIL_SQL': 'SELECT 1'}, 'FAILED select exit 23')
    check('step-docker-failure', ['sh', str(scripts['step.sh']), 'pg', 'select', 'bench', 'SELECT 1'], 29, {'FAIL_DOCKER': '1'})
    check('bulk-success', ['sh', str(scripts['bulk.sh']), 'pg', 'pg-1'], 0, contains='peak_mb')
    check('bulk-step-failure', ['sh', str(scripts['bulk.sh']), 'pg', 'pg-1'], 23, {'FAIL_SQL': '\\copy bulk from'}, 'FAILED copy_in exit 23')
    # crun's adapter contract is checked separately; the fake bulk prevents engine/perf probes.
    scripts['bulk.sh'].write_text('#!/bin/sh\n[ "${FAIL_BULK:-}" != 1 ] || exit 24\necho FAKE_BULK_OK\n')
    for name, extra, expected in [('crun-success', {}, 0), ('crun-bulk-failure', {'FAIL_BULK': '1'}, 24), ('crun-run-failure', {'FAIL_RUN': '1'}, 31)]:
        target = out / (name + '.raw')
        check(name, ['sh', str(scripts['crun.sh']), '1', str(target)], expected, extra)
        if expected == 0:
            assert target.read_text().endswith('done\n') and target.read_text().count('FAKE_BULK_OK') == 2
(out / 'checks.json').write_text(json.dumps({'cases': cases, 'services_started': 0,
    'benchmarks_run': 0, 'source_sha256': {n: hashlib.sha256((source/n).read_bytes()).hexdigest()
    for n in ['step.sh','bulk.sh','crun.sh']}, 'note': 'Fixture-only cgroup paths and run adapter; fake Docker only. No completed timing or acceptance rule changed.'}, indent=2))
