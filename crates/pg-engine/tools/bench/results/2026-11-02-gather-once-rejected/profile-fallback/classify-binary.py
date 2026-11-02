"""Disjoint period-weighted ancestry buckets using original ELF mappings.

Recovered planning buckets are lower bounds: finite stack snapshots can omit
ancestors. Replay decoder symbol labels are never consumed. No phase µs/op is
derived, because sampled windows and whole-client accounting differ.
"""
from pathlib import Path
import bisect
from collections import Counter, defaultdict
import hashlib
import json
import re
import struct
import subprocess
import sys

run = Path(sys.argv[1]).resolve()
binary = Path(sys.argv[2])
assert hashlib.sha256(binary.read_bytes()).hexdigest() == sys.argv[3]
symbols = []
for line in subprocess.check_output(['nm', '-n', '-S', '-C', str(binary)], text=True).splitlines():
    match = re.match(r'^([0-9a-f]+) ([0-9a-f]+) ([tT]) (.*)$', line)
    if match:
        symbols.append((int(match[1], 16), int(match[2], 16), match[4]))
starts = [s[0] for s in symbols]


def original_bias(data):
    assert data[:8] == b'PERFILE2'
    start, size = struct.unpack_from('<QQ', data, 40)
    offset, biases = start, set()
    samples, lost = [], 0
    attr_offset = struct.unpack_from('<Q', data, 24)[0]
    sample_type = struct.unpack_from('<Q', data, attr_offset + 24)[0]
    assert sample_type & 1
    while offset < start + size:
        kind, misc, length = struct.unpack_from('<IHH', data, offset)
        assert length >= 8
        if kind == 9:
            ip_offset = offset + 8 + (8 if sample_type & (1 << 16) else 0)
            sample_ip, sample_pid, sample_tid, sample_time = struct.unpack_from('<QIIQ', data, ip_offset)
            samples.append((sample_time, sample_ip, sample_pid, sample_tid))
        elif kind == 2:
            lost += struct.unpack_from('<Q', data, offset + 16)[0]
        elif kind == 13:
            lost += struct.unpack_from('<Q', data, offset + 8)[0]
        if kind == 10:
            name = data[offset + 72:offset + length].split(b'\0')[0].decode()
            address, span, pgoff = struct.unpack_from('<QQQ', data, offset + 16)
            if name == str(binary) and pgoff == 0:
                biases.add(address)
        offset += length
    assert len(biases) == 1 and offset == start + size
    return biases.pop(), samples, lost


def symbolize(address, bias):
    va = int(address, 16) - bias
    index = bisect.bisect_right(starts, va) - 1
    if index >= 0:
        start, size, name = symbols[index]
        if start <= va < start + size:
            return {'ip': address, 'elf_va': hex(va), 'symbol': name, 'offset': hex(va - start)}
    return {'ip': address, 'symbol': None}


results = []
for stacks in sorted(run.glob('*-stacks.txt')):
    if stacks.name.startswith('experimental-'):
        continue
    label = stacks.name.removesuffix('-stacks.txt')
    original = run / (label + '.data')
    if not original.exists():
        continue
    bias, original_ips, lost = original_bias(original.read_bytes())
    assert lost == 0, (label, lost)
    replay = json.loads((run / (label + '-replay.json')).read_text())
    assert hashlib.sha256(original.read_bytes()).hexdigest() == replay['original_sha256']
    records = []
    for text in stacks.read_text().strip().split('\n\n'):
        lines = text.splitlines()
        period = re.search(r'\s(\d+) cpu-clock:', lines[0])
        assert period, lines[0]
        ips = [line.strip().split()[0] for line in lines[1:] if line.startswith('\t')]
        assert ips
        frames = [symbolize(ip, bias) for ip in ips]
        names = [f['symbol'] or '' for f in frames]
        root = any(name == 'pg_engine::session::plan_cost_estimate' or name == '<pg_engine::session::Session>::plan_cost' for name in names)
        binding = any(name == '<pg_engine::session::Session>::bound_custom' for name in names)
        assert not (root and binding), 'overlapping conceptual buckets need inspection'
        if binding:
            bucket = 'custom_binding'
        elif root:
            bucket = 'root_costing'
        elif any(name == '<pg_engine::session::Session>::planned_as' for name in names):
            bucket = 'other_planning'
        elif any(name.startswith('pg_engine::exec::run_each') for name in names):
            bucket = 'execution_and_row_output'
        elif 'pg_engine::server::message_loop' in names:
            bucket = 'other_session_and_wire'
        else:
            bucket = 'unresolved_or_background'
        records.append({'header': lines[0], 'period': int(period[1]), 'bucket': bucket, 'frames': frames})
    assert len(records) == len(original_ips)
    original_ips.sort()
    for record, (sample_time, sample_ip, sample_pid, sample_tid) in zip(records, original_ips):
        assert int(record['frames'][0]['ip'], 16) == sample_ip
        header = re.search(r' (\d+)/(\d+)\s+\[\d+\] (\d+\.\d+):', record['header'])
        assert header and int(header[1]) == sample_pid and int(header[2]) == sample_tid
        assert abs(float(header[3]) * 1e9 - sample_time) <= 1001
    counts = Counter(record['bucket'] for record in records)
    weights = Counter()
    for record in records:
        weights[record['bucket']] += record['period']
    total = sum(weights.values())
    summary = {'label': label, 'samples': len(records), 'lost_samples': lost, 'raw_sample_ips_match': True, 'total_period': total,
               'counts': dict(counts), 'period_weight': dict(weights),
               'fractions': {key: value / total for key, value in weights.items()},
               'original_load_bias': hex(bias),
               'physical_depths': dict(sorted(Counter(len(record['frames']) for record in records).items()))}
    (run / (label + '-classified.json')).write_text(json.dumps({'summary': summary, 'samples': records}, indent=2) + '\n')
    results.append(summary)
(run / 'classification-summary.json').write_text(json.dumps({
    'limits': 'Diagnostic sampled CPU ancestry with synthetic mapping replay. Finite stacks truncate. Named planning weights are lower bounds, unresolved weight is retained. No throughput claim or phase microseconds per operation.',
    'captures': results
}, indent=2) + '\n')
for item in results:
    print(item['label'], item['samples'], {key: round(value * 100, 2) for key, value in item['fractions'].items()})
