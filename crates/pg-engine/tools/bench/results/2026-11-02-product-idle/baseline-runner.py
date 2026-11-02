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


def snapshot(group):
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
        except FileNotFoundError:
            continue
    return {'monotonic_ns': stamp, 'cpu': cpu, 'memory': memory,
            'memory_current': int((group / 'memory.current').read_text()),
            'io_stat': (group / 'io.stat').read_text(), 'threads': threads,
            'snapshot_finished_ns': time.monotonic_ns()}


def measure(target, seconds):
    group = Path(target['cgroup'])
    before = snapshot(group)
    time.sleep(seconds)
    after = snapshot(group)
    elapsed = (after['monotonic_ns'] - before['monotonic_ns']) / 1e9
    cpu = after['cpu']['usage_usec'] - before['cpu']['usage_usec']
    if elapsed < seconds or cpu < 0:
        raise RuntimeError('invalid resource accounting interval')
    common = {tid for tid in before['threads'].keys() & after['threads'].keys()
              if before['threads'][tid]['start_ticks'] == after['threads'][tid]['start_ticks']}
    switches = sum(after['threads'][tid]['voluntary'] - before['threads'][tid]['voluntary']
                   for tid in common)
    if switches < 0:
        raise RuntimeError('thread identity or counters changed')
    return {'before': before, 'after': after, 'elapsed_s': elapsed,
            'cpu_us_per_s': cpu / elapsed,
            'voluntary_switches_per_s': switches / elapsed,
            'thread_ids_unchanged': common == before['threads'].keys() == after['threads'].keys(),
            'memory_anon_shmem_mib': (after['memory']['anon'] + after['memory']['shmem']) / 2**20,
            'memory_current_mib': after['memory_current'] / 2**20}


def validate(config):
    if not re.fullmatch(r'bench_idle_[a-z0-9_]+', config['db']):
        raise ValueError('use a dedicated database named bench_idle_<unique suffix>')
    for key in ['rounds', 'sessions']:
        if type(config[key]) is not int or config[key] <= 0:
            raise ValueError(key + ' must be a positive integer')
    for key in ['seconds', 'settle_seconds', 'release_seconds']:
        if not isinstance(config[key], (int, float)) or not math.isfinite(config[key]) or config[key] <= 0:
            raise ValueError(key + ' must be positive and finite')
    if config['seconds'] > 60:
        raise ValueError('each observation must be at most 60 seconds')
    targets = config['targets']
    names = [item['name'] for item in targets]
    if not targets or len(set(names)) != len(names) or any(not re.fullmatch(r'[a-z0-9_-]+', n) for n in names):
        raise ValueError('target names must be unique safe identifiers')
    for target in targets:
        if type(target['port']) is not int or not 1 <= target['port'] <= 65535:
            raise ValueError('invalid target port')
        for name in ['cpu.stat', 'memory.stat', 'memory.current', 'io.stat', 'cgroup.threads']:
            if not (Path(target['cgroup']) / name).is_file():
                raise ValueError('missing cgroup v2 accounting file: ' + name)
    if (not config['phases'] or len(set(config['phases'])) != len(config['phases'])
            or any(phase not in ['disconnected', 'connected', 'released'] for phase in config['phases'])):
        raise ValueError('unsupported idle lifecycle phase')


def run(config):
    validate(config)
    out = Path(config['out'])
    out.mkdir(parents=True, exist_ok=False)
    save(out / 'config.json', config)
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
                _, version = query(conn, 'SELECT version()')
                _, settings = query(conn, 'SELECT name, setting, unit, source FROM pg_settings ORDER BY name')
                save(out / (target['name'] + '-settings.json'), {'version': version_to_text(version),
                     'settings': version_to_text(settings), 'cgroup_procs': (Path(target['cgroup']) / 'cgroup.procs').read_text(),
                     'effective_cpus': (Path(target['cgroup']) / 'cpuset.cpus.effective').read_text()})
                query(conn, 'CREATE TABLE idle_probe_v1 (id integer PRIMARY KEY, balance bigint NOT NULL, note text NOT NULL)')
                query(conn, "INSERT INTO idle_probe_v1 SELECT i, i::bigint * 7919, 'idle ' || i FROM generate_series(1, 100) i")
                check(conn)
                operations += 1
            finally:
                conn.s.close()
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
                    conn.s.close()
        summary = {}
        for phase, targets in results.items():
            summary[phase] = {}
            for name, records in targets.items():
                measured = [r for r in records if not r['warmup']]
                summary[phase][name] = {}
                for metric in ['cpu_us_per_s', 'voluntary_switches_per_s', 'memory_anon_shmem_mib', 'memory_current_mib']:
                    values = [r[metric] for r in measured]
                    summary[phase][name][metric] = {'median': statistics.median(values), 'min': min(values), 'max': max(values)}
        save(out / 'summary.json', summary)
        save(out / 'complete.json', {'checked_fixture_queries': operations, 'errors': 0})
        print(json.dumps(summary, indent=2), flush=True)
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
