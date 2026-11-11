"""Retained-stack diagnostic only; timings from profiled work are not benchmark claims."""
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
from datetime import datetime, timezone

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')
DIAG = Path('/home/meszmate/.cache/pg-engine-perf-20261102/planning-diagnostic/diagnose.py')
spec = importlib.util.spec_from_file_location('diag', DIAG)
d = importlib.util.module_from_spec(spec); spec.loader.exec_module(d)
wire = d.load_module('wire', d.WIRE)
paired = d.load_module('paired', d.RUNNER)
run = ROOT / ('capture-' + datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ'))
run.mkdir()
for src in [Path(__file__), DIAG, ROOT / 'normalize-maps.py', d.WIRE, d.RUNNER]:
    (run / src.name).write_bytes(src.read_bytes())
profiler = '813b04eed73f'
engines, runtimes = {}, {}
profiler_started = False


def save(name, value):
    (run / name).write_text(json.dumps(value, indent=2) + '\n')


def call(argv, log=None, timeout=120):
    if log:
        with (run / log).open('w') as f:
            return subprocess.run(argv, text=True, stdout=f, stderr=subprocess.STDOUT, check=True, timeout=timeout)
    return subprocess.check_output(argv, text=True, timeout=timeout)


def fixture(port):
    c = wire.Conn(port, db=d.DB); c.s.settimeout(120)
    try:
        d.query(c, "SET datestyle='ISO,YMD'")
        value = '|'.join(x.decode() for x in d.query(c, "SELECT count(*),sum(id::bigint),sum(owner::bigint),sum(balance::numeric),md5(string_agg(id::text||':'||owner::text||':'||balance::text||':'||note||':'||created::text,',' ORDER BY id)) FROM accounts")[0])
        assert value == d.DIGEST
        return value
    finally:
        c.s.close()


def capture(label, mode, clients, record):
    runtime = runtimes[mode]
    base = ['taskset', '-c', '32-35', str(d.CLIENT), '--target', 'wire', '--label', label, '--port', str(runtime['port']), '--db', d.DB, '--mem-pid', str(runtime['pid']), 'oltp', '--rows', '200000', '--only', 'range_scan_100', '--clients', str(clients), '--secs', '30' if record else '10']
    before = paired.cpu({'pid': runtime['pid']}, 100)
    started = time.monotonic()
    commands = []
    with (run / (label + '-client.log')).open('w') as f:
        client = subprocess.Popen(base, stdout=f, stderr=subprocess.STDOUT)
        try:
            if record:
                time.sleep(2)
                data = run / (label + '.data')
                command = ['docker', 'exec', profiler, 'perf', 'record', '-e', 'cpu-clock', '-F', '49', '-T', '--sample-cpu', '--call-graph', 'dwarf,65528', '-p', str(runtime['pid']), '-o', str(data), '--', 'sleep', '20']
                commands.append(command)
                call(command, label + '-record.log', 60)
                call(['docker', 'exec', profiler, 'chmod', '644', str(data)])
        finally:
            try:
                client.wait(timeout=40)
            except subprocess.TimeoutExpired:
                client.terminate(); client.wait(timeout=5)
    assert client.returncode == 0
    values = paired.parse((run / (label + '-client.log')).read_text(), label, 'range_scan_100', clients)
    save(label + '-invocation.json', {'command': base, 'exit': client.returncode, 'result': values, 'elapsed_parent_s': time.monotonic() - started, 'server_cpu_s': paired.cpu({'pid': runtime['pid']}, 100) - before, 'profiled': record, 'record_commands': commands})
    if record:
        replay = run / (label + '-replay.data')
        call(['python3', str(ROOT / 'normalize-maps.py'), str(data), str(d.ENGINE), str(replay)])
        call(['docker', 'exec', profiler, 'perf', 'script', '-f', '-i', str(replay), '-F', 'comm,pid,tid,time,cpu,event,ip,period', '--no-inline'], label + '-stacks.txt', 180)
        call(['docker', 'exec', profiler, 'perf', 'report', '-i', str(data), '--stdio', '--header-only'], label + '-header.txt', 60)
    print('Completed ' + label, flush=True)


try:
    os.chdir(REPO)
    for path, digest in d.HASHES.items(): assert d.sha(path) == digest, path
    assert d.command(['git', 'rev-parse', 'HEAD:crates/pg-engine/src']) == d.TREE
    before = json.loads(call(['docker', 'inspect', profiler]))[0]
    assert not before['State']['Running']
    assert before['HostConfig']['Memory'] == before['HostConfig']['MemorySwap'] == 536870912
    assert before['Config']['Labels']['agent.worktree'] == 't3-d4020b98'
    save('profiler-before.json', before)
    d.require_stopped(d.FIXTURE)
    files = d.tree_manifest(d.FIXTURE); save('source-fixture-files.json', files)
    save('inputs.json', {'at_utc': d.stamp(), 'source': d.SOURCE, 'production_tree': d.TREE, 'hashes': {str(p): h for p, h in d.HASHES.items()}, 'head': d.command(['git', 'rev-parse', 'HEAD']), 'status': d.command(['git', 'status', '--porcelain']), 'load': Path('/proc/loadavg').read_text(), 'claim': 'Steady-state sampled planning ancestry only; no optimization or profiled throughput claim.', 'limits': 'Synthetic replay fixes libdw PIE offset; raw IPs must be symbolized against original maps/ELF. Finite stack snapshots still truncate. Whole-invocation ops differ from the sample window.'})
    call(['docker', 'update', '--memory', '2g', '--memory-swap', '2g', profiler])
    profiler_started = True
    call(['docker', 'start', profiler])
    for i, mode in enumerate(['custom', 'generic', 'auto']):
        data = run / (mode + '-data'); port = 53332 + i
        d.require_stopped(d.FIXTURE)
        call(['cp', '-a', '--reflink=auto', str(d.FIXTURE), str(data)])
        assert d.tree_manifest(data) == files
        with socket.socket() as s:s.bind(('127.0.0.1', port))
        argv = ['taskset', '-c', '28-31', str(d.ENGINE), '--data', str(data), '--listen', f'127.0.0.1:{port}', '--password', 'pw', '--cache-mb', '2048', '-c', 'maintenance_work_mem=256MB', '-c', 'max_connections=100', '-c', 'plan_cache_mode=' + d.MODES[mode]]
        with (run / (mode + '-server.log')).open('w') as f:engine = subprocess.Popen(argv, stdout=f, stderr=subprocess.STDOUT)
        engines[mode] = engine
        save(mode + '-owned.json', {'pid': engine.pid, 'argv': argv})
        for attempt in range(120):
            assert engine.poll() is None
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=.5):break
            except OSError:time.sleep(.25)
        else:raise RuntimeError('engine startup failed')
        runtimes[mode] = d.runtime(engine, port, mode, data, argv)
        save(mode + '-runtime.json', runtimes[mode])
        save(mode + '-preflight.json', d.preflight(wire, port, mode))
        save(mode + '-fixture-before.json', fixture(port))
    for mode in ['custom', 'generic', 'auto']:
        capture('unprofiled-' + mode, mode, 8 if mode == 'auto' else 1, False)
    for round_no in range(3):
        for mode in (['custom', 'generic'] if round_no % 2 == 0 else ['generic', 'custom']):
            capture(f'{round_no}-{mode}-1', mode, 1, True)
    for round_no in range(3):capture(f'{round_no}-auto-8', 'auto', 8, True)
    for mode, runtime in runtimes.items():save(mode + '-fixture-after.json', fixture(runtime['port']))
    assert d.tree_manifest(d.FIXTURE) == files
    save('complete.json', {'at_utc': d.stamp(), 'diagnostic_only': True})
finally:
    cleanup = []
    for mode, engine in engines.items():
        try:
            d.stop(engine)
            cleanup.append({'mode': mode, 'pid': engine.pid, 'exit': engine.returncode, 'stopped': True})
        except Exception as e:cleanup.append({'mode': mode, 'pid': engine.pid, 'error': repr(e), 'stopped': engine.poll() is not None})
    if profiler_started:
        try:call(['docker', 'stop', '-t', '10', profiler])
        finally:call(['docker', 'update', '--memory', '512m', '--memory-swap', '512m', profiler])
    info = json.loads(call(['docker', 'inspect', profiler]))[0]
    cleanup.append({'profiler': info['Id'], 'running': info['State']['Running'], 'memory': info['HostConfig']['Memory'], 'swap': info['HostConfig']['MemorySwap']})
    save('cleanup.json', cleanup)
    assert not info['State']['Running'] and info['HostConfig']['Memory'] == info['HostConfig']['MemorySwap'] == 536870912
    print('Cleanup saved: ' + str(run), flush=True)
