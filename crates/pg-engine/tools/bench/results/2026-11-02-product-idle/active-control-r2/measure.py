"""Checked acceptance experiment, five alternating pairs plus warmup."""
from pathlib import Path
import importlib.util
import json
import os
import signal
import socket
import subprocess
import sys
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')
path = Path('/home/meszmate/.cache/pg-engine-perf-20261102/planning-diagnostic/diagnose.py')
spec = importlib.util.spec_from_file_location('diag', path)
d = importlib.util.module_from_spec(spec); spec.loader.exec_module(d)
wire = d.load_module('wire', d.WIRE)
engines = []
helper = None


def save(name, value):
    with (ROOT / name).open('x') as f:
        json.dump(value, f, indent=2); f.write('\n')


def interrupted(signum, frame):
    raise KeyboardInterrupt(f'signal {signum}')


signal.signal(signal.SIGTERM, interrupted)
try:
    os.chdir(REPO)
    assert not d.command(['git', 'status', '--porcelain'])
    candidate = ROOT.parent / 'candidate-bin/roll-engine'
    build = json.loads((ROOT.parent / 'candidate-bin/manifest.json').read_text())
    assert d.command(['git', 'rev-parse', 'HEAD:crates/pg-engine/src']) == build['production_tree']
    assert d.sha(candidate) == build['binary_sha256']
    assert d.sha(d.ENGINE) == d.HASHES[d.ENGINE] and d.sha(d.CLIENT) == d.HASHES[d.CLIENT]
    d.require_stopped(d.FIXTURE)
    files = d.tree_manifest(d.FIXTURE)
    save('source-fixture-files.json', files)
    helper = d.command(['docker', 'run', '-d', '--rm', '--name', 't3-d4020b98-product-idle-active-psql', '--label', 'agent.worktree=t3-d4020b98', '--label', 'agent.task=product-idle-active', '--memory', '256m', '--network', 'host', '--entrypoint', '/bin/sh', d.IMAGE, '-c', 'exec sleep infinity'])
    save('helper.json', {'id': helper, 'image': d.IMAGE})
    targets, runtimes, preflights = [], {}, []
    for slot, (name, binary, source, expected) in enumerate([
        ('baseline', d.ENGINE, d.SOURCE, d.HASHES[d.ENGINE]),
        ('candidate', candidate, build['source'], build['binary_sha256'])
    ]):
        data = ROOT / (name + '-data'); port = 53370 + slot
        d.require_stopped(d.FIXTURE)
        d.command(['cp', '-a', '--reflink=auto', d.FIXTURE, data])
        assert d.tree_manifest(data) == files
        with socket.socket() as s:s.bind(('127.0.0.1', port))
        argv = ['taskset', '-c', '28-31', str(binary), '--data', str(data), '--listen', f'127.0.0.1:{port}', '--password', 'pw', '--cache-mb', '2048', '-c', 'maintenance_work_mem=256MB', '-c', 'max_connections=100']
        with (ROOT / (name + '-server.log')).open('x') as f:
            engine = subprocess.Popen(argv, stdout=f, stderr=subprocess.STDOUT)
        engines.append((name, engine))
        save(name + '-owned.json', {'pid': engine.pid, 'argv': argv})
        for attempt in range(120):
            assert engine.poll() is None
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=.5):break
            except OSError:time.sleep(.25)
        else:raise RuntimeError('engine not ready')
        proc = Path(f'/proc/{engine.pid}')
        runtime = {'pid': engine.pid, 'port': port, 'sha256': d.sha(proc / 'exe'), 'executable': str((proc / 'exe').resolve()), 'cmdline': (proc / 'cmdline').read_bytes().replace(b'\0', b' ').decode(), 'affinity': sorted(os.sched_getaffinity(engine.pid)), 'thread_affinities': sorted({tuple(os.sched_getaffinity(int(t.name))) for t in (proc / 'task').iterdir()}), 'cgroup': (proc / 'cgroup').read_text()}
        assert runtime['sha256'] == expected and runtime['affinity'] == [28,29,30,31] and runtime['thread_affinities'] == [(28,29,30,31)]
        runtimes[name] = runtime
        preflight = d.preflight(wire, port, 'auto'); save(name + '-preflight.json', preflight)
        preflights.append(preflight)
        targets.append({'name': name, 'port': port, 'probe': {'pid': engine.pid}, 'source': source})
    assert preflights[0]['cases'] == preflights[1]['cases'], 'plans/counters differ'
    assert [[row for row in p['settings'] if row[0] != 'port'] for p in preflights][0] == [[row for row in p['settings'] if row[0] != 'port'] for p in preflights][1], 'settings differ'
    save('runtime.json', {'at_utc': d.stamp(), 'source': build['source'], 'production_tree': build['production_tree'], 'load': Path('/proc/loadavg').read_text(), 'engines': runtimes, 'client_sha256': d.sha(d.CLIENT), 'expected_fixture': d.DIGEST})
    config = {'bench': str(d.CLIENT), 'out': str(ROOT / 'candidate-vs-baseline'), 'db': d.DB, 'rows': 200000, 'rounds': 5, 'seconds': 20, 'client_cpus': '32-35', 'cases': {'range_scan_100': [8]}, 'psql_container': helper, 'expected_fixture': d.DIGEST, 'runtime_provenance': 'runtime.json', 'targets': targets}
    save('candidate-vs-baseline.json', config)
    with (ROOT / 'paired-console.log').open('x') as f:
        subprocess.run(['python3', str(d.RUNNER), str(ROOT / 'candidate-vs-baseline.json')], stdout=f, stderr=subprocess.STDOUT, timeout=600, check=True)
    assert d.tree_manifest(d.FIXTURE) == files
    save('complete.json', {'at_utc': d.stamp(), 'source_fixture_unchanged': True})
finally:
    cleanup = []
    for name, engine in engines:
        try:
            d.stop(engine)
            cleanup.append({'name': name, 'pid': engine.pid, 'exit': engine.returncode, 'stopped': True})
        except Exception as e:
            cleanup.append({'name': name, 'pid': engine.pid, 'stopped': engine.poll() is not None, 'error': repr(e)})
    if helper:
        result = subprocess.run(['docker', 'rm', '-f', helper], text=True, capture_output=True, timeout=30)
        cleanup.append({'helper_id': helper, 'remove_exit': result.returncode, 'stderr': result.stderr})
    save('cleanup.json', cleanup)
