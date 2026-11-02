#!/usr/bin/env python3
"""Checked idle lifecycle measurements against existing, dedicated servers (Linux/cgroup v2)."""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import socket
import statistics
import struct
import sys
import time

sys.dont_write_bytecode = True
WIRE = Path(__file__).resolve().parents[2] / 'tests/clients/wire.py'
spec = importlib.util.spec_from_file_location('wire', WIRE)
wire = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wire)


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def query(conn, sql):
    conn.s.settimeout(10)
    conn.query(sql)
    rows, oids = [], []
    while True:
        kind, body = conn._msg()
        if kind == b'E':
            raise RuntimeError(wire.decode(kind, body))
        if kind == b'T':
            count = struct.unpack_from('!h', body)[0]
            pos, oids = 2, []
            for _ in range(count):
                end = body.index(b'\0', pos)
                oids.append(struct.unpack_from('!I', body, end + 7)[0])
                pos = end + 19
        if kind == b'D':
            count = struct.unpack_from('!h', body)[0]
            pos, values = 2, []
            for _ in range(count):
                size = struct.unpack_from('!i', body, pos)[0]
                pos += 4
                values.append(None if size == -1 else body[pos:pos + size])
                if size >= 0:
                    pos += size
            rows.append(values)
        if kind == b'Z':
            if body != b'I':
                raise RuntimeError('connection did not return to idle transaction state')
            return oids, rows


def connect(target, db):
    return wire.Conn(target['port'], user=target.get('user', 'postgres'),
                     password=target.get('password', 'pw'), db=db)


def close(connections):
    for conn in connections:
        try:
            conn.send(b'X', b'')
        except OSError:
            pass
        finally:
            conn.s.close()
    connections.clear()


def check(conn):
    actual = query(conn, 'SELECT id, balance, note FROM idle_probe_v1 ORDER BY id')
    expected = [[str(i).encode(), str(i * 7919).encode(), f'idle {i}'.encode()]
                for i in range(1, 101)]
    if actual != ([23, 20, 25], expected):
        raise RuntimeError('idle fixture types, ordering, or values differ')


def fields(path):
    return {key: int(value) for key, value in (line.split() for line in path.read_text().splitlines())}


def optional_text(path):
    return path.read_text() if path.exists() else None


def process_start(pid):
    return int(Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[19])


def snapshot(target):
    group = Path(target['cgroup'])
    if process_start(target['leader_pid']) != target['leader_start_ticks']:
        raise RuntimeError('server process identity changed')
    stamp = time.monotonic_ns()
    cpu = fields(group / 'cpu.stat')
    memory = fields(group / 'memory.stat')
    tids = [int(tid) for tid in (group / 'cgroup.threads').read_text().split()]
    threads = {}
    for tid in tids:
        proc = Path(f'/proc/{tid}')
        try:
            status = dict(line.split(':', 1) for line in (proc / 'status').read_text().splitlines())
            threads[str(tid)] = {
                'name': status['Name'].strip(),
                'start_ticks': int((proc / 'stat').read_text().rsplit(')', 1)[1].split()[19]),
                'voluntary': int(status['voluntary_ctxt_switches']),
                'involuntary': int(status['nonvoluntary_ctxt_switches']),
            }
        except (FileNotFoundError, ProcessLookupError):
            continue
    return {'monotonic_ns': stamp, 'cpu': cpu, 'memory': memory,
            'memory_current': int((group / 'memory.current').read_text()),
            'memory_swap_current': int((group / 'memory.swap.current').read_text()),
            'memory_events': fields(group / 'memory.events'),
            'io_stat': optional_text(group / 'io.stat'), 'threads': threads,
            'snapshot_finished_ns': time.monotonic_ns()}


def measure(target, seconds):
    before = snapshot(target)
    time.sleep(seconds)
    after = snapshot(target)
    elapsed = (after['monotonic_ns'] - before['monotonic_ns']) / 1e9
    cpu = after['cpu']['usage_usec'] - before['cpu']['usage_usec']
    if elapsed < seconds or cpu < 0:
        raise RuntimeError('invalid resource accounting interval')
    common = {tid for tid in before['threads'].keys() & after['threads'].keys()
              if before['threads'][tid]['start_ticks'] == after['threads'][tid]['start_ticks']}
    deltas = [after['threads'][tid]['voluntary'] - before['threads'][tid]['voluntary'] for tid in common]
    if not common or any(delta < 0 for delta in deltas):
        raise RuntimeError('thread identity or counters changed')
    def io_totals(value):
        totals = dict.fromkeys(['rbytes', 'wbytes', 'rios', 'wios'], 0)
        for line in value.splitlines():
            values = dict(item.split('=') for item in line.split()[1:])
            for key in totals:
                totals[key] += int(values.get(key, 0))
        return totals
    io_delta = None
    if before['io_stat'] is not None and after['io_stat'] is not None:
        io_before, io_after = [io_totals(s['io_stat']) for s in [before, after]]
        io_delta = {key: io_after[key] - io_before[key] for key in io_before}
    if io_delta is not None and any(value < 0 for value in io_delta.values()):
        raise RuntimeError('I/O counters reset')
    return {'before': before, 'after': after, 'elapsed_s': elapsed,
            'cpu_us': cpu, 'block_io_delta': io_delta,
            'cpu_us_per_s': cpu / elapsed,
            'voluntary_switches_surviving_threads_per_s': sum(deltas) / elapsed,
            'surviving_threads': len(common),
            'removed_threads': len(before['threads']) - len(common),
            'added_threads': len(after['threads']) - len(common),
            'thread_ids_unchanged': common == before['threads'].keys() == after['threads'].keys(),
            'memory_anon_shmem_mib': (after['memory']['anon'] + after['memory']['shmem']) / 2**20,
            'memory_current_mib': after['memory_current'] / 2**20}


def validate(config):
    if len(config['db'].encode()) > 63 or not re.fullmatch(r'bench_idle_[a-z0-9_]+', config['db']):
        raise ValueError('use a dedicated database named bench_idle_<unique suffix>')
    for key in ['rounds', 'sessions']:
        if type(config[key]) is not int or config[key] <= 0:
            raise ValueError(key + ' must be a positive integer')
    for key in ['seconds', 'settle_seconds', 'release_seconds']:
        if not isinstance(config[key], (int, float)) or not math.isfinite(config[key]) or config[key] <= 0:
            raise ValueError(key + ' must be positive and finite')
    if config['seconds'] > 60:
        raise ValueError('each observation must be at most 60 seconds')
    if not config.get('diagnostic_only', False) and (config['rounds'] < 5 or config['seconds'] < 10 or config['settle_seconds'] < 3):
        raise ValueError('comparison requires at least five rounds, ten-second windows and three-second settling')
    targets = config['targets']
    names = [item['name'] for item in targets]
    if not targets or len(set(names)) != len(names) or any(not re.fullmatch(r'[a-z0-9_-]+', n) for n in names):
        raise ValueError('target names must be unique safe identifiers')
    groups = [Path(target['cgroup']).resolve() for target in targets]
    if len(set(groups)) != len(groups) or len({t['port'] for t in targets}) != len(targets):
        raise ValueError('targets must have distinct cgroups and ports')
    own_group = Path('/sys/fs/cgroup' + Path('/proc/self/cgroup').read_text().strip().split('::', 1)[1])
    limits = []
    for target in targets:
        if type(target['port']) is not int or not 1 <= target['port'] <= 65535:
            raise ValueError('invalid target port')
        if target['kind'] not in ['engine', 'postgres'] or type(target['leader_pid']) is not int:
            raise ValueError('invalid server identity')
        group = Path(target['cgroup']).resolve()
        if any(group in other.parents for other in groups) or group == own_group or group in own_group.parents:
            raise ValueError('target cgroups must not overlap each other or the observer')
        for name in ['cpu.stat', 'memory.stat', 'memory.current', 'memory.swap.current', 'memory.events',
                     'cgroup.threads', 'cgroup.procs',
                     'memory.max', 'memory.swap.max', 'cpu.max', 'cpu.weight']:
            if not (Path(target['cgroup']) / name).is_file():
                raise ValueError('missing cgroup v2 accounting file: ' + name)
        if target['leader_pid'] not in map(int, (group / 'cgroup.procs').read_text().split()):
            raise ValueError('expected server leader is outside target cgroup')
        if process_start(target['leader_pid']) != target['leader_start_ticks']:
            raise ValueError('expected server leader restarted')
        limits.append({key: (group / key).read_text().strip()
                       for key in ['memory.max', 'memory.swap.max', 'cpu.max', 'cpu.weight']})
        for tid in (group / 'cgroup.threads').read_text().split():
            if sorted(os.sched_getaffinity(int(tid))) != config['server_cpus']:
                raise ValueError('server thread affinity differs from configured CPUs')
    if any(limit != limits[0] for limit in limits):
        raise ValueError('server cgroup resource limits differ')
    if (not config['phases'] or len(set(config['phases'])) != len(config['phases'])
            or any(phase not in ['disconnected', 'connected', 'released'] for phase in config['phases'])):
        raise ValueError('unsupported idle lifecycle phase')


def run(config):
    validate(config)
    out = Path(config['out'])
    out.mkdir(parents=True, exist_ok=False)
    save(out / 'config.json', {**config, 'targets': [{k: v for k, v in t.items() if k != 'password'} for t in config['targets']]})
    save(out / 'identity.json', {'runner_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                               'wire_sha256': hashlib.sha256(WIRE.read_bytes()).hexdigest(),
                               'load_at_start': Path('/proc/loadavg').read_text().strip(),
                               'client_affinity': sorted(os.sched_getaffinity(0)),
                               'cpu_accounting_unit': 'microseconds; zero observed is not zero actual CPU'})
    held = {target['name']: [] for target in config['targets']}
    results, operations = {}, 0
    try:
        for target in config['targets']:
            conn = connect(target, config['db'])
            try:
                if query(conn, 'SELECT current_database()')[1] != [[config['db'].encode()]]:
                    raise RuntimeError('connected to a different database')
                if target['kind'] == 'postgres':
                    backend = int(query(conn, 'SELECT pg_backend_pid()')[1][0][0])
                    pids = (Path(target['cgroup']) / 'cgroup.procs').read_text().split()
                    namespace_pids = []
                    for pid in pids:
                        try:
                            status = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/status').read_text().splitlines())
                            namespace_pids.append(int(status['NSpid'].split()[-1]))
                        except (FileNotFoundError, ProcessLookupError):
                            continue
                    if backend not in namespace_pids:
                        raise RuntimeError('connected PostgreSQL backend is outside target cgroup')
                else:
                    args = Path(f'/proc/{target["leader_pid"]}/cmdline').read_bytes().split(b'\0')
                    if f'127.0.0.1:{target["port"]}'.encode() not in args:
                        raise RuntimeError('engine process arguments do not match listener')
                _, version = query(conn, 'SELECT version()')
                _, settings = query(conn, 'SELECT name, setting, unit, source FROM pg_settings ORDER BY name')
                save(out / (target['name'] + '-settings.json'), {'version': version_to_text(version),
                     'settings': version_to_text(settings), 'cgroup_procs': (Path(target['cgroup']) / 'cgroup.procs').read_text(),
                     'limits': {key: (Path(target['cgroup']) / key).read_text().strip()
                                for key in ['memory.max', 'memory.swap.max', 'cpu.max', 'cpu.weight']},
                     'effective_cpus': optional_text(Path(target['cgroup']) / 'cpuset.cpus.effective'),
                     'io_controller_available': (Path(target['cgroup']) / 'io.stat').exists()})
                query(conn, 'CREATE TABLE idle_probe_v1 (id integer PRIMARY KEY, balance bigint NOT NULL, note text NOT NULL)')
                query(conn, "INSERT INTO idle_probe_v1 SELECT i, i::bigint * 7919, 'idle ' || i FROM generate_series(1, 100) i")
                check(conn)
                operations += 1
            finally:
                close([conn])
        for phase in config['phases']:
            for target in config['targets']:
                close(held[target['name']])
                if phase == 'connected':
                    for _ in range(config['sessions']):
                        conn = connect(target, config['db'])
                        held[target['name']].append(conn)
                        check(conn)
                        operations += 1
            time.sleep(config['release_seconds'] if phase != 'connected' else config['settle_seconds'])
            for round_no in range(config['rounds'] + 1):
                targets = config['targets'] if round_no % 2 == 0 else list(reversed(config['targets']))
                for target in targets:
                    connections = held[target['name']]
                    for conn in connections:
                        check(conn)
                        operations += 1
                    if connections:
                        time.sleep(config['settle_seconds'])
                    record = measure(target, config['seconds'])
                    record.update({'phase': phase, 'round': round_no, 'warmup': round_no == 0,
                                   'target': target['name'], 'connections_held': len(connections)})
                    record['minimum_seconds_since_fixture_query'] = config['settle_seconds'] if connections else None
                    save(out / f'{phase}-{round_no:02d}-{target["name"]}.json', record)
                    results.setdefault(phase, {}).setdefault(target['name'], []).append(record)
                    for conn in connections:
                        check(conn)
                        operations += 1
            for target in config['targets']:
                conn = connect(target, config['db'])
                try:
                    check(conn)
                    operations += 1
                finally:
                    close([conn])
        summary = {}
        for phase, targets in results.items():
            summary[phase] = {}
            for name, records in targets.items():
                measured = [r for r in records if not r['warmup']]
                summary[phase][name] = {}
                summary[phase][name]['time_weighted_cpu_us_per_s'] = sum(r['cpu_us'] for r in measured) / sum(r['elapsed_s'] for r in measured)
                summary[phase][name]['switch_claim_valid_without_churn'] = all(r['thread_ids_unchanged'] for r in measured)
                summary[phase][name]['thread_churn_windows'] = sum(not r['thread_ids_unchanged'] for r in measured)
                summary[phase][name]['added_threads_total'] = sum(r['added_threads'] for r in measured)
                summary[phase][name]['removed_threads_total'] = sum(r['removed_threads'] for r in measured)
                summary[phase][name]['block_io_totals'] = (
                    {key: sum(r['block_io_delta'][key] for r in measured) for key in ['rbytes', 'wbytes', 'rios', 'wios']}
                    if all(r['block_io_delta'] is not None for r in measured) else None)
                for metric in ['cpu_us_per_s', 'voluntary_switches_surviving_threads_per_s', 'memory_anon_shmem_mib', 'memory_current_mib']:
                    values = [r[metric] for r in measured]
                    summary[phase][name][metric] = {'median': statistics.median(values), 'min': min(values), 'max': max(values)}
        if 'connected' in summary and 'disconnected' in summary:
            summary['per_connection_increment_estimates_mib'] = {
                name: {metric: (summary['connected'][name][metric]['median'] - summary['disconnected'][name][metric]['median']) / config['sessions']
                       for metric in ['memory_anon_shmem_mib', 'memory_current_mib']}
                for name in summary['connected']}
        save(out / 'summary.json', summary)
        save(out / 'complete.json', {'checked_fixture_queries': operations, 'errors': 0})
        print(json.dumps(summary, indent=2), flush=True)
    except Exception as error:
        save(out / 'failure.json', {'error': repr(error), 'checked_fixture_queries': operations})
        raise
    finally:
        for connections in held.values():
            close(connections)


def version_to_text(rows):
    return [[None if value is None else value.decode() for value in row] for row in rows]


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config', type=Path)
    args = parser.parse_args()
    socket.setdefaulttimeout(10)
    run(json.loads(args.config.read_text()))
