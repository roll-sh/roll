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
import threading

sys.dont_write_bytecode = True
import argparse
parser = argparse.ArgumentParser()
parser.add_argument('--out', type=Path, required=True)
parser.add_argument('--seconds', type=int, default=10)
parser.add_argument('--rounds', type=int, default=5)
parser.add_argument('--port0', type=int, default=53440)
parser.add_argument('--range-only', action='store_true')
parser.add_argument('--candidate-dir', type=Path, default=Path('/home/meszmate/.cache/pg-engine-perf-20261102/scan-display/candidate-bin'))
parser.add_argument('--observe-host', action='store_true')
args = parser.parse_args()
ROOT = args.out.resolve()
ROOT.mkdir(parents=True, exist_ok=False)
EXPERIMENT = Path('/home/meszmate/.cache/pg-engine-perf-20261102/scan-display')
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')
path = Path('/home/meszmate/.cache/pg-engine-perf-20261102/planning-diagnostic/diagnose.py')
spec = importlib.util.spec_from_file_location('diag', path)
d = importlib.util.module_from_spec(spec); spec.loader.exec_module(d)
wire = d.load_module('wire', d.WIRE)
engines = []
helper = None
observer_stop = threading.Event()
observer = None
observer_errors = []


def save(name, value):
    with (ROOT / name).open('x') as f:
        json.dump(value, f, indent=2); f.write('\n')


def interrupted(signum, frame):
    raise KeyboardInterrupt(f'signal {signum}')


signal.signal(signal.SIGTERM, interrupted)
try:
    os.chdir(REPO)
    candidate = args.candidate_dir.resolve() / 'roll-engine'
    build = json.loads((args.candidate_dir.resolve() / 'manifest.json').read_text())
    assert all(d.sha(REPO / path) == digest for path, digest in build['source_sha256'].items())
    assert d.sha(candidate) == build['binary_sha256']
    assert d.sha(d.ENGINE) == d.HASHES[d.ENGINE] and d.sha(d.CLIENT) == d.HASHES[d.CLIENT]
    d.require_stopped(d.FIXTURE)
    files = d.tree_manifest(d.FIXTURE)
    save('source-fixture-files.json', files)
    if args.observe_host:
        frequency_paths = {str(c): Path(f'/sys/devices/system/cpu/cpu{c}/cpufreq/scaling_cur_freq') for c in range(10, 18)}
        frequency_paths.update({str(c): Path(f'/sys/devices/system/cpu/cpu{c}/cpufreq/scaling_cur_freq') for c in range(28, 36)})
        for path in frequency_paths.values():
            path.read_text()
        def observe():
            try:
                with (ROOT / 'host-cpu-observation.jsonl').open('x') as log:
                    while not observer_stop.is_set():
                        cpus = {line.split()[0]: [int(n) for n in line.split()[1:]] for line in Path('/proc/stat').read_text().splitlines() if line.startswith('cpu')}
                        record = {'monotonic': time.monotonic(), 'at_unix': time.time(), 'cpu': cpus, 'load': Path('/proc/loadavg').read_text().strip(), 'freq_khz': {c: p.read_text().strip() for c, p in frequency_paths.items()}}
                        log.write(json.dumps(record) + '\n'); log.flush()
                        observer_stop.wait(1)
            except Exception as e:
                observer_errors.append(repr(e))
        observer = threading.Thread(target=observe, daemon=True)
        observer.start()
    helper = d.command(['docker', 'run', '-d', '--rm', '--name', 't3-d4020b98-scan-display-psql-402446', '--label', 'agent.worktree=t3-d4020b98', '--label', 'agent.task=scan-display', '--memory', '256m', '--network', 'host', '--entrypoint', '/bin/sh', d.IMAGE, '-c', 'exec sleep infinity'])
    save('helper.json', {'id': helper, 'image': d.IMAGE})
    targets, runtimes, preflights = [], {}, []
    for slot, (name, binary, source, expected) in enumerate([
        ('baseline', d.ENGINE, d.SOURCE, d.HASHES[d.ENGINE]),
        ('candidate', candidate, build['source'], build['binary_sha256'])
    ]):
        data = ROOT / (name + '-data'); port = args.port0 + slot
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
    save('runtime.json', {'at_utc': d.stamp(), 'source': build['source'], 'source_sha256': build['source_sha256'], 'load': Path('/proc/loadavg').read_text(), 'engines': runtimes, 'client_sha256': d.sha(d.CLIENT), 'expected_fixture': d.DIGEST})
    config = {'bench': str(d.CLIENT), 'out': str(ROOT / 'candidate-vs-baseline'), 'db': d.DB, 'rows': 200000, 'rounds': args.rounds, 'seconds': args.seconds, 'client_cpus': '32-35', 'cases': ({'range_scan_100': [8]} if args.range_only else {'range_scan_100': [8], 'point_select': [1]}), 'psql_container': helper, 'expected_fixture': d.DIGEST, 'runtime_provenance': 'runtime.json', 'targets': targets}
    save('candidate-vs-baseline.json', config)
    with (ROOT / 'paired-console.log').open('x') as f:
        subprocess.run(['python3', str(d.RUNNER), str(ROOT / 'candidate-vs-baseline.json')], stdout=f, stderr=subprocess.STDOUT, timeout=4*(args.rounds+1)*(args.seconds+30)+120, check=True)
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
    if observer is not None:
        observer_stop.set()
        observer.join(timeout=5)
        save('host-observation.json', {'errors': observer_errors, 'stopped': not observer.is_alive(), 'scope': 'Descriptive host CPU and frequency observation; not exclusive CPU ownership or profiling; starts before server launch and warmup.'})
        assert not observer.is_alive() and not observer_errors
    save('cleanup.json', cleanup)
