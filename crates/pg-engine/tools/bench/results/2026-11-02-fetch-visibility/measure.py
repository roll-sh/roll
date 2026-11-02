"""Prospective Fetch visibility setup trial and dirty-heap fallback control."""

def check(condition, message):
    if not condition:
        raise RuntimeError(message)

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
import struct
import resource
import statistics

sys.dont_write_bytecode = True
import argparse
parser = argparse.ArgumentParser()
parser.add_argument('--out', type=Path, required=True)
parser.add_argument('--port0', type=int, default=53700)
parser.add_argument('--candidate-dir', type=Path, default=Path('/home/meszmate/.cache/pg-engine-perf-20261102/fetched-fast-path/candidate-bin'))
parser.add_argument('--control', choices=['dirty'])
parser.add_argument('--monitor', action='store_true')
parser.add_argument('--swap-slots', action='store_true')
parser.add_argument('--preflight-only', action='store_true')
args = parser.parse_args()
args.decline = args.control is not None
args.rounds = 5 if args.decline else 12
args.seconds = 15 if args.decline else 20
args.observe_host = True
check(not args.preflight_only or args.monitor, 'Validation failed in measure.py, original line 29')
ROOT = args.out.resolve()
ROOT.mkdir(parents=True, exist_ok=False)
EXPERIMENT = Path('/home/meszmate/.cache/pg-engine-perf-20261102/fetched-fast-path')
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



def transaction_query(c, sql, expected):
    c.query(sql)
    frames = []
    while True:
        kind, body = c._msg()
        check(kind != b'E', 'SQL error in transaction-lock check: ' + repr(body))
        frames.append((kind, body))
        if kind == b'Z':
            check(body == expected, 'unexpected transaction state: ' + repr(body))
            return d.rows(frames)


def interrupted(signum, frame):
    raise KeyboardInterrupt(f'signal {signum}')


signal.signal(signal.SIGTERM, interrupted)
save('started.json', {'at_unix': time.time(), 'pid': os.getpid(), 'monotonic': time.monotonic()})
try:
    os.chdir(REPO)
    candidate = args.candidate_dir.resolve() / 'roll-engine'
    build = json.loads((args.candidate_dir.resolve() / 'manifest.json').read_text())
    check(all(d.sha(REPO / path) == digest for path, digest in build['source_sha256'].items()), 'Validation failed in measure.py, original line 127')
    check(d.sha(candidate) == build['binary_sha256'], 'Validation failed in measure.py, original line 128')
    check(d.sha(d.ENGINE) == d.HASHES[d.ENGINE] and d.sha(d.CLIENT) == d.HASHES[d.CLIENT], 'Validation failed in measure.py, original line 129')
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
    helper = d.command(['docker', 'run', '-d', '--rm', '--name', f't3-d4020b98-fetched-fast-path-psql-{os.getpid()}', '--label', 'agent.worktree=t3-d4020b98', '--label', 'agent.task=fetched-fast-path', '--memory', '256m', '--network', 'host', '--entrypoint', '/bin/sh', d.IMAGE, '-c', 'exec sleep infinity'])
    save('helper.json', {'id': helper, 'image': d.IMAGE})
    targets, runtimes, preflights = [], {}, []
    assignments = [
        ('baseline', d.ENGINE, d.SOURCE, d.HASHES[d.ENGINE]),
        ('candidate', candidate, build['source'], build['binary_sha256'])
    ]
    if args.swap_slots:
        assignments.reverse()
    for slot, (name, binary, source, expected) in enumerate(assignments):
        data = ROOT / (name + '-data'); port = args.port0 + slot
        d.require_stopped(d.FIXTURE)
        d.command(['cp', '-a', '--reflink=auto', d.FIXTURE, data])
        check(d.tree_manifest(data) == files, 'Validation failed in measure.py, original line 160')
        with socket.socket() as s:s.bind(('127.0.0.1', port))
        argv = ['taskset', '-c', '28-31', str(binary), '--data', str(data), '--listen', f'127.0.0.1:{port}', '--password', 'pw', '--cache-mb', '2048', '-c', 'maintenance_work_mem=256MB', '-c', 'max_connections=100', '-c', 'autovacuum=off']
        if args.monitor:
            argv += ['-c', 'shared_preload_libraries=pg_stat_statements', '-c', 'pg_stat_statements.track_planning=on']
        with (ROOT / (name + '-server.log')).open('x') as f:
            engine = subprocess.Popen(argv, stdout=f, stderr=subprocess.STDOUT)
        engines.append((name, engine))
        save(name + '-owned.json', {'pid': engine.pid, 'argv': argv})
        for attempt in range(120):
            check(engine.poll() is None, 'Validation failed in measure.py, original line 172')
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=.5):break
            except OSError:time.sleep(.25)
        else:raise RuntimeError('engine not ready')
        proc = Path(f'/proc/{engine.pid}')
        runtime = {'pid': engine.pid, 'port': port, 'sha256': d.sha(proc / 'exe'), 'executable': str((proc / 'exe').resolve()), 'cmdline': (proc / 'cmdline').read_bytes().replace(b'\0', b' ').decode(), 'affinity': sorted(os.sched_getaffinity(engine.pid)), 'thread_affinities': sorted({tuple(os.sched_getaffinity(int(t.name))) for t in (proc / 'task').iterdir()}), 'cgroup': (proc / 'cgroup').read_text(), 'nice': int((proc / 'stat').read_text().rsplit(')', 1)[1].split()[16])}
        check(runtime['sha256'] == expected and runtime['affinity'] == [28,29,30,31] and runtime['thread_affinities'] == [(28,29,30,31)], 'Validation failed in measure.py, original line 179')
        check(runtime['nice'] == 19, 'unexpected engine nice level')
        runtimes[name] = runtime
        if args.monitor:
            d.pgss_query(wire, port, 'CREATE EXTENSION pg_stat_statements')
            d.pgss_query(wire, port, 'SELECT pg_stat_statements_reset()')
        d.pgss_query(wire, port, 'CREATE EXTENSION IF NOT EXISTS pg_visibility')
        d.pgss_query(wire, port, 'ALTER TABLE accounts SET (autovacuum_enabled = off)')
        if args.control == 'dirty':
            update = wire.Conn(port, db=d.DB)
            update.s.settimeout(180)
            try:
                d.query(update, 'UPDATE accounts SET balance=balance')
            finally:
                update.s.close()
        visibility = d.pgss_query(wire, port, "SELECT pg_relation_size('accounts'::regclass)/8192, all_visible FROM pg_visibility_map_summary('accounts'::regclass)")
        check(len(visibility) == 1 and int(visibility[0][0]) > 0, 'missing heap visibility')
        check(int(visibility[0][1]) == (0 if args.control == 'dirty' else int(visibility[0][0])), 'wrong visibility phase')
        save(name + '-visibility-before.json', visibility)
        preflight = d.preflight(wire, port, 'auto'); save(name + '-preflight.json', preflight)
        preflights.append(preflight)
        if args.monitor:
            stats = d.pgss_query(wire, port, "SELECT query,calls,rows,parallel_workers_to_launch,parallel_workers_launched FROM pg_stat_statements WHERE query LIKE 'SELECT id,%FROM accounts%' ORDER BY query")
            check(len(stats) == 2 and sum(int(row[2]) for row in stats) >= 2020, 'Validation failed in measure.py, original line 188')
            check(all(row[3:] == ['0', '0'] for row in stats), 'Validation failed in measure.py, original line 189')
            c = wire.Conn(port, db=d.DB); c.s.settimeout(30)
            try:
                transaction_query(c, 'BEGIN', b'T')
                got = transaction_query(c, 'SELECT id,balance FROM accounts WHERE id=20', b'T')
                check(got == [[b'20', str(20*7919 % 100000).encode()]], 'Validation failed in measure.py, original line 194')
                locks = d.text_rows(transaction_query(c, "SELECT mode,granted FROM pg_locks WHERE pid=pg_backend_pid() AND relation='accounts'::regclass ORDER BY mode", b'T'))
                check(locks == [['AccessShareLock', 't']], 'Validation failed in measure.py, original line 196')
                transaction_query(c, 'ROLLBACK', b'I')
                released = d.text_rows(d.query(c, "SELECT count(*) FROM pg_locks WHERE pid=pg_backend_pid() AND relation='accounts'::regclass"))
                check(released == [['0']], 'Validation failed in measure.py, original line 199')
                check(d.text_rows(d.query(c, "SELECT count(*) FROM pg_stat_activity WHERE backend_type='parallel worker'")) == [['0']], 'Validation failed in measure.py, original line 200')
            finally:
                c.s.close()
            save(name + '-monitor-correctness.json', {'pgss': stats, 'transaction_locks': locks, 'locks_after_rollback': released})
        targets.append({'name': name, 'port': port, 'probe': {'pid': engine.pid}, 'source': source})
    check(preflights[0]['cases'] == preflights[1]['cases'], 'plans/counters differ')
    check([[row for row in p['settings'] if row[0] != 'port'] for p in preflights][0] == [[row for row in p['settings'] if row[0] != 'port'] for p in preflights][1], 'settings differ')
    if args.monitor:
        check(json.loads((ROOT / 'baseline-monitor-correctness.json').read_text()) == json.loads((ROOT / 'candidate-monitor-correctness.json').read_text()), 'Validation failed in measure.py, original line 208')
    save('runtime.json', {'at_utc': d.stamp(), 'source': build['source'], 'source_sha256': build['source_sha256'], 'load': Path('/proc/loadavg').read_text(), 'engines': runtimes, 'client_sha256': d.sha(d.CLIENT), 'expected_fixture': d.DIGEST, 'fallback_phase': args.decline, 'control_phase': args.control, 'slots_swapped': args.swap_slots, 'fetch_visibility_phase': 'dirty' if args.control else 'all-visible'})
    config = {'bench': str(d.CLIENT), 'out': str(ROOT / 'candidate-vs-baseline'), 'db': d.DB, 'rows': 200000, 'rounds': args.rounds, 'seconds': args.seconds, 'client_cpus': '32-35', 'cases': {'range_scan_100': [8], 'point_select': [1]}, 'psql_container': helper, 'expected_fixture': d.DIGEST, 'runtime_provenance': 'runtime.json', 'targets': targets}
    save('candidate-vs-baseline.json', config)
    if args.preflight_only:
        pairs = d.load_module('preflight_pairs', d.RUNNER)
        digests = {target['name']: pairs.fixture(config, target) for target in targets}
        check(all(value == d.DIGEST for value in digests.values()), 'diagnostic fixture values differ')
        save('diagnostic-fixture-digests.json', digests)
    with (ROOT / 'paired-console.log').open('x') as f:
        if not args.preflight_only:
            subprocess.run(['python3', str(d.RUNNER), str(ROOT / 'candidate-vs-baseline.json')], stdout=f, stderr=subprocess.STDOUT, timeout=4*(args.rounds+1)*(args.seconds+30)+120, check=True)
    for target in targets:
        visibility = d.pgss_query(wire, target['port'], "SELECT pg_relation_size('accounts'::regclass)/8192, all_visible FROM pg_visibility_map_summary('accounts'::regclass)")
        check(len(visibility) == 1 and int(visibility[0][0]) > 0, 'missing final visibility')
        check(int(visibility[0][1]) == (0 if args.control == 'dirty' else int(visibility[0][0])), 'visibility phase changed')
        save(target['name'] + '-visibility-after.json', visibility)
    check(d.tree_manifest(d.FIXTURE) == files, 'original fixture changed')
    save('complete.json', {'at_utc': d.stamp(), 'source_fixture_unchanged': True, 'benchmark_executed': not args.preflight_only, 'monitor_preflight_checked_queries_per_side': 40 if args.preflight_only else None})
finally:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    cleanup = []
    for name, engine in engines:
        try:
            d.stop(engine)
            cleanup.append({'name': name, 'pid': engine.pid, 'exit': engine.returncode, 'stopped': True})
        except Exception as e:
            cleanup.append({'name': name, 'pid': engine.pid, 'stopped': engine.poll() is not None, 'error': repr(e)})
    if helper:
        try:
            result = subprocess.run(['docker', 'rm', '-f', helper], text=True, capture_output=True, timeout=30)
            cleanup.append({'helper_id': helper, 'remove_exit': result.returncode, 'stderr': result.stderr})
        except Exception as error:
            cleanup.append({'helper_id': helper, 'error': repr(error)})
    if observer is not None:
        observer_stop.set()
        observer.join(timeout=5)
    save('cleanup.json', cleanup)
    if observer is not None:
        save('host-observation.json', {'errors': observer_errors, 'stopped': not observer.is_alive(), 'scope': 'Descriptive host CPU and frequency observation; not exclusive CPU ownership or profiling; starts before server launch and warmup.'})
        check(not observer.is_alive() and not observer_errors, 'Validation failed in measure.py, original line 238')

    check(all(row.get('stopped') and row.get('exit') == 0 for row in cleanup if 'pid' in row), 'Owned engine cleanup failed; inspect cleanup.json')
    check(all(row.get('remove_exit') == 0 for row in cleanup if 'helper_id' in row), 'Owned helper cleanup failed; inspect cleanup.json')
