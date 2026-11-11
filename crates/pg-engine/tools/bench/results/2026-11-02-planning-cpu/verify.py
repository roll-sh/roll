"""Read-only portable checks; no profiler, executable, or server is needed."""
from datetime import date, timedelta
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import statistics
import sys

sys.dont_write_bytecode = True
root = Path(__file__).resolve().parent

def read(path):
    return json.loads(path.read_text())

for line in (root / 'SHA256SUMS').read_text().splitlines():
    digest, name = line.split('  ', 1)
    assert hashlib.sha256((root / name).read_bytes()).hexdigest() == digest, name
manifest = read(root / 'manifest.json')
spec = importlib.util.spec_from_file_location('paired', root / 'paired_oltp.py')
paired = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paired)
assert hashlib.sha256((root / 'paired_oltp.py').read_bytes()).hexdigest() == manifest['runner_sha256']
inputs = read(root / 'inputs.json')
assert inputs['status'] == ''
assert inputs['production_tree'] == manifest['production_tree']
assert manifest['engine_sha256'] in inputs['hashes'].values()
assert manifest['client_sha256'] in inputs['hashes'].values()
# Independently regenerate the deterministic SQL digest.
h = hashlib.md5()
owners = balances = 0
for i in range(1, 200001):
    owner, balance = i % 50000, i * 7919 % 100000
    owners += owner
    balances += balance
    h.update(((',' if i > 1 else '') + f'{i}:{owner}:{balance}:note {i}:{date(2024, 1, 1) + timedelta(days=i % 365)}').encode())
fixture = f'200000|20000100000|{owners}|{balances}|{h.hexdigest()}'
assert fixture == manifest['expected_fixture']
operations = invocations = 0
for path in sorted(root.glob('*-invocation.json')):
    item = read(path)
    label = path.name.removesuffix('-invocation.json')
    clients = 8 if 'auto' in label else 1
    result = paired.parse((root / (label + '-client.log')).read_text(), label, 'range_scan_100', clients)
    assert item['exit'] == 0 and result == item['result'] and item['server_cpu_s'] > 0
    operations += int(result['ops'])
    invocations += 1
assert invocations == 12 and operations == 1360591
for mode in ['auto', 'custom', 'generic']:
    runtime = read(root / f'{mode}-runtime.json')
    assert runtime['sha256'] == manifest['engine_sha256']
    assert runtime['affinity'] == [28, 29, 30, 31]
    assert runtime['thread_affinities'] == [[28, 29, 30, 31]]
    for stage in ['before', 'after']:
        assert read(root / f'{mode}-fixture-{stage}.json') == fixture
    preflight = read(root / f'{mode}-preflight.json')
    for case, values in preflight['cases'].items():
        for i, counts in enumerate(values['counters'], 1):
            expected = ([i, 0] if mode == 'generic' else [0, i] if mode == 'custom' or case == 'range_scan_100'
                        else [max(0, i - 5), min(5, i)])
            assert counts == expected
boundaries = read(root / 'boundary-symbols.json')
assert len(boundaries) == 4
saved = {item['label']: item for item in read(root / 'classification-summary.json')['captures']}
samples = 0
for label, expected in saved.items():
    bias = int(expected['original_load_bias'], 16)
    counts = {'root_costing': 0, 'custom_binding': 0, 'other': 0}
    weights = dict(counts)
    for block in (root / (label + '-stacks.txt')).read_text().strip().split('\n\n'):
        lines = block.splitlines()
        match = re.search(r'\s(\d+) cpu-clock:', lines[0])
        assert match
        period = int(match[1])
        assert period > 0
        addresses = [int(line.strip(), 16) - bias for line in lines[1:] if line.startswith('\t')]
        names = {item['symbol'] for item in boundaries for address in addresses
                 if item['start'] <= address < item['start'] + item['size']}
        binding = '<pg_engine::session::Session>::bound_custom' in names
        costing = bool(names & {'<pg_engine::session::Session>::plan_cost', 'pg_engine::session::plan_cost_estimate'})
        assert not (binding and costing)
        bucket = 'custom_binding' if binding else 'root_costing' if costing else 'other'
        counts[bucket] += 1
        weights[bucket] += period
    assert sum(counts.values()) == expected['samples']
    assert sum(weights.values()) == expected['total_period']
    assert expected['lost_samples'] == 0 and expected['raw_sample_ips_match']
    for bucket in ['root_costing', 'custom_binding']:
        assert counts[bucket] == expected['counts'].get(bucket, 0)
        assert weights[bucket] == expected['period_weight'].get(bucket, 0)
    replay = read(root / (label + '-replay.json'))
    assert replay['binary_sha256'] == manifest['engine_sha256'] and replay['synthetic_replay_only']
    assert len(replay['changes']) == 3
    assert [v['new_pgoff'] - v['old_pgoff'] for v in replay['changes']] == [4096, 8192, 12288]
    samples += expected['samples']
assert len(saved) == 9 and samples == 16157
cleanup = read(root / 'cleanup.json')
assert all(item['stopped'] and item['exit'] == 0 for item in cleanup if 'pid' in item)
profiler = cleanup[-1]
assert not profiler['running'] and profiler['memory'] == profiler['swap'] == 536870912
live = read(root / 'final-live-cleanup.json')
assert all(item['pid_absent'] and item['connection_refused'] for item in live['engines'])
print(f'Verified {invocations} invocations, {operations} checked operations, {samples} decoded samples, ancestry weights, fixtures, and recorded cleanup')
