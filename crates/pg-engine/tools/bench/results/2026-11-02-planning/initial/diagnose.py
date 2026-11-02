#!/usr/bin/env python3
"""Parent-run diagnostic; this file was designed, not experimentally executed.

Run from any directory:
  ./diagnose.py --run
  ./diagnose.py --run --phase     # also collect separate instrumented phase evidence

No compilation or source changes. Two owned engines use independent reflink/copy
clones of the SAME stopped, previously checked fixture and the SAME histogram
binary. The unchanged frozen client is passed to unchanged paired_oltp.py.
Default: auto/generic and auto/custom, each one discarded warmup pair followed
by five alternating measured pairs, 10 s/case, range at 1 and 8 clients plus
point at 1 client. About 12 minutes of timed client work, plus startup/digests.
--phase adds a separate generic/custom pass, range at 1 client, with pgss on;
those results must never be pooled with the clean CPU measurements.

Claim allowed: observed mode-sensitive server CPU/op and checked wire latency.
Not allowed: mode delta == pure planning/costing CPU, or a production mode change.
EXPLAIN operators may match while Param versus Const bounds and $1+99 folding
change expression/executor work. The prior range plans estimated generic cost
43.42 / 1000 rows versus custom 10.24 / 91 rows. Keep verbose bounds/output in
the artifacts; check index direction, filters, projections and sorts, not only
the index name. Costs/row estimates are estimates, not timings.
pgss times the real run_query planning span (includes plan selection/costing,
not just root estimator); it excludes Parse/Describe and is ELAPSED, not CPU.
In this engine total_exec_time INCLUDES the planning span. Report plan/call,
exec/call, and (exec-plan)/call; do not add them. The plans counter measures
timed span entries, not actual custom-plan count. Check prepared counters below.
At >1 client summed pgss wall times overlap. Use the 1-client phase pass to
interpret phase share, with instrumentation overhead and timer/store costs.
No phase result singles out plan_cost_estimate; a separate symbol profiler is
needed for that attribution. perf was absent and perf_event_paranoid=3 at design.
No profiler installation or kernel-setting change is attempted. If a profiler
is made available later, sample separate checked range runs and distinguish
planned_as/bound_custom/plan_cost_estimate from scan/expression/serialization
stacks before interpreting a CPU share. Also account for setup/client CPU.

Require a quiescent shared host on CPUs 28-35; the script records load/affinity
but cannot reserve CPUs. Both servers share CPUs 28-31, only one gets benchmark
traffic at a time; client CPUs 32-35. Abort rather than change pinned controls.
Longer/follow-up runs or a CPU/fixture role swap are needed for close/noisy gaps.
Compare medians AND min/max, paired per-round deltas; differences inside spread
are inconclusive. Point auto should become generic, so it controls for startup
forcing and protocol overhead; custom is expected to change its steady state.

Cleanup SIGTERMs only Popen-owned engines, escalates to SIGINT, and removes only
the helper ID this run creates. Data/results remain under this new owned run
directory for audit. Never kill a port occupant or touch original fixtures.
On uncatchable interruption, use runtime-*.json / cleanup.json to identify owned
PIDs and paths; verify /proc/PID/exe AND --data before stopping them manually.
Only remove this run's data dirs after its engines are confirmed stopped.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import sys
import time
from datetime import datetime, timezone

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')
OLD = Path.home() / '.cache/pg-engine-perf-20261101/oltp-20261102'
TRIAL = Path.home() / '.cache/pg-engine-perf-20261102/btree-selectivity'
FIXTURE = TRIAL / 'baseline-data'
ENGINE = OLD / 'histogram-bin/roll-engine'
CLIENT = OLD / 'final-client/bench'
RUNNER = REPO / 'crates/pg-engine/tools/bench/paired_oltp.py'
WIRE = REPO / 'crates/pg-engine/tests/clients/wire.py'
DB = 'perf_oltp20261102_clean_200000'
DIGEST = '200000|20000100000|4999900000|9999900000|ac8ccd2a44331a8aa083be3b8e06ee5f'
SOURCE = '5b32046dee8c9908e2bb25c892ff60df54c33b33'
TREE = 'bfaf631ee83440b27a036b7de1be253636a241df'
HASHES = {
    ENGINE: '5cfc79043a50d0ac228bdac3c66deb5fcf0dd34d73add389bf2773237e51ffd1',
    CLIENT: '9f5b02222402fd5b09772088ce704054299339bb80723f1ef15fd915fb0b8e28',
    RUNNER: '03542748e7c220c3d15e3221b4951bc7fec0834710ae50ee9b0154ea464ac46c',
    WIRE: 'ecb0dde8aa6bda441a642a11a71f6ea5cfc1d1d8b79a8a74e8653b37e61e698b',
}
IMAGE = 'postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873'
MODES = {'auto': 'auto', 'generic': 'force_generic_plan', 'custom': 'force_custom_plan'}
SQL = {
    'range_scan_100': 'SELECT id, balance FROM accounts WHERE id BETWEEN $1 AND $1 + 99 ORDER BY id',
    'point_select': 'SELECT id, owner, balance, note, created FROM accounts WHERE id = $1',
}


def check(ok, message):
    if not ok:
        raise RuntimeError(message)


def sha(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as f:
        for block in iter(lambda: f.read(4 * 1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def command(args, **kw):
    return subprocess.check_output([str(a) for a in args], text=True, timeout=120, **kw).strip()


def save(path, value):
    with path.open('x') as f:
        json.dump(value, f, indent=2)
        f.write('\n')


def stamp():
    return datetime.now(timezone.utc).isoformat()


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def tree_manifest(path):
    files = {}
    for p in sorted(path.rglob('*')):
        check(not p.is_symlink(), 'fixture contains symlink: ' + str(p))
        if p.is_file():
            files[str(p.relative_to(path))] = [p.stat().st_size, sha(p)]
    check(bool(files), 'empty fixture')
    return files


def require_stopped(path):
    # Parent must retain exclusive ownership of this stopped fixture during copy.
    # Inspect accessible processes without relying on stale PID files alone.
    for proc in Path('/proc').iterdir():
        if not proc.name.isdigit():
            continue
        try:
            argv = (proc / 'cmdline').read_bytes().split(b'\0')
            for i, arg in enumerate(argv[:-1]):
                if arg == b'--data':
                    check(Path(os.fsdecode(argv[i+1])).resolve() != path.resolve(),
                          'fixture is in use by PID ' + proc.name)
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue


def drain(c):
    frames = []
    while True:
        t, b = c._msg()
        check(t != b'E', 'SQL/protocol error: ' + repr(b))
        frames.append((t, b))
        if t == b'Z':
            check(b == b'I', 'connection did not return to idle')
            return frames


def rows(frames):
    out = []
    for t, b in frames:
        if t != b'D':
            continue
        n = struct.unpack('!h', b[:2])[0]
        pos, row = 2, []
        for _ in range(n):
            size = struct.unpack('!i', b[pos:pos+4])[0]
            pos += 4
            row.append(None if size == -1 else b[pos:pos+size])
            pos += max(size, 0)
        check(pos == len(b), 'bad DataRow')
        out.append(row)
    return out


def query(c, sql):
    c.query(sql)
    return rows(drain(c))


def text_rows(raw):
    return [[None if v is None else v.decode() for v in row] for row in raw]


def preflight(wire, port, mode):
    c = wire.Conn(port, db=DB, params={'application_name': 'planning-diagnostic-preflight'})
    c.s.settimeout(30)
    try:
        settings = text_rows(query(c, 'SELECT name,setting,unit,source FROM pg_settings ORDER BY name'))
        check([r[1] for r in settings if r[0] == 'plan_cache_mode'] == [MODES[mode]],
              'startup plan mode did not reach a fresh connection')
        result = {'settings': settings, 'cases': {}}
        for case, sql in SQL.items():
            c.parse('oltp_probe', sql, [23])
            c.describe('S', 'oltp_probe')
            c.sync()
            parsed = drain(c)
            check([b for t, b in parsed if t == b't'] == [struct.pack('!hI', 1, 23)],
                  'wrong inferred input type')
            counters = []
            for i in range(20):
                g = [1, 50000, 199900][i % 3]
                c.bind('', 'oltp_probe', [struct.pack('!i', g)], formats=[1], result_formats=[1])
                c.describe('P', '')
                c.execute('', 0)
                c.sync()
                got = rows(drain(c))
                if case == 'range_scan_100':
                    expected = [[struct.pack('!i', j), struct.pack('!q', j*7919 % 100000)]
                                for j in range(g, g+100)]
                else:
                    expected = [[struct.pack('!i', g), struct.pack('!i', g % 50000),
                                 struct.pack('!q', g*7919 % 100000), f'note {g}'.encode(),
                                 struct.pack('!i', 8766 + g % 365)]]
                check(got == expected, 'preflight value mismatch')
                ct = text_rows(query(c, "SELECT generic_plans,custom_plans FROM pg_prepared_statements WHERE name='oltp_probe'"))
                check(len(ct) == 1, 'missing prepared counters')
                counters.append(list(map(int, ct[0])))
            expected_counts = ([20, 0] if mode == 'generic' else [0, 20] if mode == 'custom'
                               else [0, 20] if case == 'range_scan_100' else [15, 5])
            check(counters[-1] == expected_counts,
                  'plan policy changed: ' + repr((mode, case, counters[-1], expected_counts)))
            # Keep EXPLAIN away from activation/value-counter checks; use its own statement.
            c.close('S', 'oltp_probe')
            c.sync()
            drain(c)
            query(c, 'PREPARE explain_probe(int) AS ' + sql)
            plans = []
            for g in [1, 50000, 199900]:
                doc = json.loads(query(c, f'EXPLAIN (VERBOSE, FORMAT JSON) EXECUTE explain_probe({g})')[0][0])
                p = doc[0]['Plan']
                check(p.get('Node Type') == 'Index Scan' and p.get('Index Name') == 'accounts_pkey'
                      and not p.get('Plans'), 'physical scan changed; inspect before measuring')
                plans.append({'value': g, 'explain': doc})
            query(c, 'DEALLOCATE explain_probe')
            result['cases'][case] = {'counters': counters, 'plans': plans}
        return result
    finally:
        c.s.close()


def pgss_query(wire, port, sql):
    c = wire.Conn(port, db=DB)
    c.s.settimeout(30)
    try:
        return text_rows(query(c, sql))
    finally:
        c.s.close()


def runtime(process, port, mode, data, argv):
    proc = Path(f'/proc/{process.pid}')
    check(sha(proc / 'exe') == HASHES[ENGINE], 'live executable hash mismatch')
    affinity = sorted(os.sched_getaffinity(process.pid))
    threads = sorted({tuple(os.sched_getaffinity(int(t.name))) for t in (proc / 'task').iterdir()})
    check(affinity == list(range(28, 32)) and threads == [tuple(range(28, 32))], 'server affinity changed')
    return {'pid': process.pid, 'port': port, 'mode': mode, 'data': str(data), 'command': argv,
            'actual_executable': str((proc / 'exe').resolve()), 'sha256': sha(proc / 'exe'),
            'cmdline': (proc / 'cmdline').read_bytes().replace(b'\0', b' ').decode(),
            'starttime_ticks': (proc / 'stat').read_text().rsplit(')', 1)[1].split()[19],
            'affinity': affinity, 'thread_affinities': threads, 'cgroup': (proc / 'cgroup').read_text()}


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            process.send_signal(signal.SIGINT)
            process.wait(timeout=30)
    check(process.returncode == 0, f'engine did not exit cleanly: {process.pid}, {process.returncode}')


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--run', action='store_true', help='explicitly execute the parent diagnostic')
    ap.add_argument('--phase', action='store_true', help='add separate pgss-instrumented generic/custom pair block')
    args = ap.parse_args()
    if not args.run:
        ap.print_help()
        return
    os.chdir(REPO)
    for path, digest in HASHES.items():
        check(sha(path) == digest, 'frozen input drifted: ' + str(path))
    check(command(['git', 'rev-parse', 'HEAD:crates/pg-engine/src']) == TREE,
          'engine production tree differs from the histogram binary')
    check(set(range(28, 36)) <= os.sched_getaffinity(0), 'pinned CPUs unavailable')
    check(json.loads((TRIAL / 'cleanup.json').read_text())['engines'][0]['stopped'],
          'no clean-stop evidence for source fixture')
    require_stopped(FIXTURE)
    command(['docker', 'image', 'inspect', IMAGE])  # fail without pulling/installing
    run = ROOT / ('run-' + datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ') + '-' + str(os.getpid()))
    run.mkdir()
    print('Diagnostic artifacts: ' + str(run), flush=True)
    (run / 'diagnose.py').write_bytes(Path(__file__).read_bytes())
    for src, name in [(RUNNER, 'paired_oltp.py'), (WIRE, 'wire.py')]:
        (run / name).write_bytes(src.read_bytes())
    wire = load_module('diagnostic_wire', run / 'wire.py')
    runner = load_module('diagnostic_runner', run / 'paired_oltp.py')
    save(run / 'inputs.json', {'at_utc': stamp(), 'hashes': {str(p): h for p, h in HASHES.items()},
        'server_source': SOURCE, 'engine_production_tree': TREE, 'parent_head': command(['git', 'rev-parse', 'HEAD']),
        'parent_status': command(['git', 'status', '--porcelain']), 'fixture': str(FIXTURE),
        'expected_fixture': DIGEST, 'client_source': '89e863fce5c92ca95c581d7daa578ddf80c9a83e',
        'load': Path('/proc/loadavg').read_text(), 'online_cpus': os.cpu_count(),
        'client_affinity': '32-35', 'server_affinity': '28-31', 'psql_image': IMAGE})
    manifest = tree_manifest(FIXTURE)
    save(run / 'stopped-fixture-files.json', manifest)
    helper, processes = None, []
    cleanup = []
    def interrupted(signum, frame):
        raise KeyboardInterrupt(f'signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    try:
        # Read existing containers, but do not reuse the parent's stopped helper.
        save(run / 'existing-worktree-containers.json', command(['docker', 'container', 'ls', '-a',
            '--filter', 'label=agent.worktree=t3-d4020b98', '--format', '{{json .}}']))
        helper = command(['docker', 'run', '-d', '--rm', '--name', 't3-d4020b98-planning-' + str(os.getpid()),
            '--label', 'agent.worktree=t3-d4020b98', '--label', 'agent.task=planning-diagnostic',
            '--memory', '256m', '--network', 'host', '--entrypoint', '/bin/sh', IMAGE, '-c', 'exec sleep infinity'])
        save(run / 'helper.json', {'id': helper, 'image': IMAGE})
        blocks = [('auto-generic', 'auto', 'generic', False), ('auto-custom', 'auto', 'custom', False)]
        if args.phase:
            blocks.append(('phase-generic-custom', 'generic', 'custom', True))
        for block, left, right, phase in blocks:
            block_dir = run / block
            block_dir.mkdir()
            active, targets, evidence = [], [], {}
            for slot, mode in enumerate([left, right]):
                data = block_dir / (mode + '-data')
                require_stopped(FIXTURE)
                command(['cp', '-a', '--reflink=auto', FIXTURE, data])
                check(tree_manifest(data) == manifest, 'clone does not match stopped fixture')
                require_stopped(FIXTURE)
                check(tree_manifest(FIXTURE) == manifest, 'source fixture changed while cloning')
                port = 53320 + slot
                with socket.socket() as s:
                    s.bind(('127.0.0.1', port))  # fail instead of disturbing an occupant
                argv = ['taskset', '-c', '28-31', str(ENGINE), '--data', str(data), '--listen',
                        f'127.0.0.1:{port}', '--password', 'pw', '--cache-mb', '2048',
                        '-c', 'maintenance_work_mem=256MB', '-c', 'max_connections=100',
                        '-c', 'plan_cache_mode=' + MODES[mode]]
                if phase:
                    argv += ['--shared-preload-libraries', 'pg_stat_statements',
                             '-c', 'pg_stat_statements.track_planning=on']
                with (block_dir / (mode + '-server.log')).open('x') as log:
                    process = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT)
                processes.append(process)
                active.append(process)
                # Persist ownership immediately, even if preflight/startup fails.
                save(block_dir / (mode + '-owned.json'), {'pid': process.pid, 'data': str(data), 'argv': argv})
                for attempt in range(120):
                    check(process.poll() is None, 'engine startup failed; inspect server log')
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=.5):
                            break
                    except OSError:
                        time.sleep(.25)
                else:
                    raise RuntimeError('engine did not become ready')
                evidence[mode] = runtime(process, port, mode, data, argv)
                save(block_dir / ('runtime-' + mode + '.json'), evidence[mode])
                save(block_dir / (mode + '-preflight.json'), preflight(wire, port, mode))
                if phase:
                    pgss_query(wire, port, 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements')
                targets.append({'name': mode, 'port': port, 'probe': {'pid': process.pid}, 'source': SOURCE})
            # All settings must agree except the assigned listen port and plan mode.
            a, b = [json.loads((block_dir / (m + '-preflight.json')).read_text())['settings'] for m in [left, right]]
            ignore = {'port', 'plan_cache_mode'}
            check([r for r in a if r[0] not in ignore] == [r for r in b if r[0] not in ignore],
                  'uncontrolled settings differ')
            if phase:
                for target in targets:
                    pgss_query(wire, target['port'], 'SELECT pg_stat_statements_reset()')
            config = {'bench': str(CLIENT), 'out': str(block_dir / 'paired'), 'db': DB, 'rows': 200000,
                'rounds': 5, 'seconds': 10, 'client_cpus': '32-35',
                'cases': {'range_scan_100': [1]} if phase else {'range_scan_100': [1, 8], 'point_select': [1]},
                'psql_container': helper, 'expected_fixture': DIGEST, 'targets': targets,
                'diagnostic_only': True, 'instrumented': phase, 'runtime_provenance': evidence}
            check(sha(CLIENT) == HASHES[CLIENT] and sha(ENGINE) == HASHES[ENGINE], 'binary drift before run')
            save(block_dir / 'config.json', config)
            with (block_dir / 'paired-console.log').open('x') as log:
                paired = subprocess.Popen(['python3', str(run / 'paired_oltp.py'), str(block_dir / 'config.json')],
                                          stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    check(paired.wait(timeout=1800) == 0, 'paired runner failed; inspect paired-console.log')
                finally:
                    # Includes any taskset/bench subprocess, even on parent-only SIGTERM.
                    try:
                        os.killpg(paired.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                    try:
                        paired.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        os.killpg(paired.pid, signal.SIGKILL)
                        paired.wait(timeout=5)
            if phase:
                for target in targets:
                    # Filter out fixture digest, metadata, EXPLAIN, and stats queries.
                    data = pgss_query(wire, target['port'],
                        "SELECT queryid,query,plans,calls,total_plan_time,total_exec_time,rows "
                        "FROM pg_stat_statements WHERE query LIKE "
                        "'SELECT id, balance FROM accounts WHERE id BETWEEN % ORDER BY id'")
                    check(len(data) == 1, 'missing/ambiguous pgss workload row')
                    row = data[0]
                    records = []
                    for p in (block_dir / 'paired').glob('*-range_scan_100-1.log'):
                        if ('-' + target['name'] + '-') in p.name:
                            records.append(runner.parse(p.read_text(), target['name'], 'range_scan_100', 1))
                    ops = sum(int(r['ops']) for r in records)
                    check(len(records) == 6 and int(row[3]) == ops and int(row[6]) == 100 * ops,
                          'pgss calls/rows do not reconcile with all checked work INCLUDING warmup')
                    check(int(row[2]) == ops, 'pgss span counter differs from checked calls')
                    plan, total = float(row[4]), float(row[5])
                    check(0 <= plan <= total and total > 0 and ops > 0, 'invalid phase accounting')
                    save(block_dir / (target['name'] + '-pgss.json'), {'columns': [
                        'queryid', 'query', 'plans', 'calls', 'total_plan_time_ms', 'total_exec_time_ms_inclusive', 'rows'],
                        'raw': row, 'includes_warmup': True, 'checked_ops': ops,
                        'planning_elapsed_us_per_call': 1000 * plan / ops,
                        'inclusive_elapsed_us_per_call': 1000 * total / ops,
                        'remaining_elapsed_us_per_call': 1000 * (total-plan) / ops,
                        'planning_fraction_of_inclusive_elapsed': plan / total,
                        'limits': 'Elapsed/instrumented; not CPU share; excludes Parse/Describe; no root-cost attribution.'})
            for process in active:
                stop(process)
            save(block_dir / 'complete.json', {'at_utc': stamp(), 'cleanly_stopped': True})
        check(tree_manifest(FIXTURE) == manifest, 'original fixture changed during diagnostic')
        save(run / 'complete.json', {'at_utc': stamp(), 'diagnostic_only': True})
    finally:
        for process in processes:
            try:
                stop(process)
                cleanup.append({'pid': process.pid, 'stopped': True, 'exit': process.returncode})
            except Exception as e:
                cleanup.append({'pid': process.pid, 'stopped': process.poll() is not None, 'error': repr(e)})
        if helper:
            result = subprocess.run(['docker', 'rm', '-f', helper], capture_output=True, text=True, timeout=30)
            cleanup.append({'helper_id': helper, 'remove_exit': result.returncode, 'stderr': result.stderr})
        save(run / 'cleanup.json', cleanup)
        print('Cleanup recorded; fixtures/results retained at ' + str(run), flush=True)


if __name__ == '__main__':
    main()
