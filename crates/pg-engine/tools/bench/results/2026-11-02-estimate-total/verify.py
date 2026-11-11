"""Read-only audit of the rejected experiment's portable records."""
from datetime import date, timedelta
import csv
import hashlib
import importlib.util
import json
from pathlib import Path
import statistics
import sys

sys.dont_write_bytecode = True
root = Path(__file__).resolve().parent

def read(path):
    return json.loads(path.read_text())

inventory = {}
for line in (root / 'SHA256SUMS').read_text().splitlines():
    digest, name = line.split('  ', 1)
    assert name not in inventory, name
    inventory[name] = digest
    assert hashlib.sha256((root / name).read_bytes()).hexdigest() == digest, name
assert set(inventory) == {str(p.relative_to(root)) for p in root.rglob('*')
                          if p.is_file() and p.name != 'SHA256SUMS'}
spec = importlib.util.spec_from_file_location('paired', root / 'paired_oltp.py')
paired = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paired)
h = hashlib.md5()
owners = balances = 0
for i in range(1, 200001):
    owner, balance = i % 50000, i * 7919 % 100000
    owners += owner; balances += balance
    h.update(((',' if i > 1 else '') + f'{i}:{owner}:{balance}:note {i}:{date(2024, 1, 1) + timedelta(days=i % 365)}').encode())
fixture = f'200000|20000100000|{owners}|{balances}|{h.hexdigest()}'
config = read(root / 'candidate-vs-baseline/config.json')
assert config['expected_fixture'] == fixture
assert config['rounds'] == 5 and config['seconds'] == 10
assert config['bench_sha256'] == '9f5b02222402fd5b09772088ce704054299339bb80723f1ef15fd915fb0b8e28'
assert config['runner_sha256'] == hashlib.sha256((root / 'paired_oltp.py').read_bytes()).hexdigest()
runtime = read(root / 'runtime.json')
assert read(root / 'candidate-binary.json')['binary_sha256'] == runtime['engines']['candidate']['sha256']
saved = {}
invocations = operations = 0
for target in config['targets']:
    name = target['name']
    observed = runtime['engines'][name]
    assert observed['sha256'] == target['actual_executable_sha256']
    assert observed['affinity'] == [28, 29, 30, 31] and observed['thread_affinities'] == [[28, 29, 30, 31]]
    for stage in ['before', 'after']:
        assert (root / f'candidate-vs-baseline/fixture-{stage}-{name}.txt').read_text().strip() == fixture
    for case, counts in config['cases'].items():
        for clients in counts:
            samples = []
            for round_no in range(6):
                stem = root / f'candidate-vs-baseline/{round_no:02d}-{name}-{case}-{clients}'
                values = paired.parse(stem.with_suffix('.log').read_text(), name, case, clients)
                resource = read(stem.with_suffix('.json'))
                assert resource['exit'] == 0
                for kind in ['server', 'client']:
                    assert resource[kind + '_cpu_s'] > 0
                    values[kind + '_cpu_us_per_op'] = resource[kind + '_cpu_s'] * 1e6 / int(values['ops'])
                invocations += 1; operations += int(values['ops'])
                if round_no:samples.append(values)
            stats = {'samples': len(samples)}
            for key in ['tps', 'p50_ms', 'p95_ms', 'p99_ms', 'server_cpu_us_per_op', 'client_cpu_us_per_op', 'peak_mem_mb']:
                numbers = [float(item[key]) for item in samples]
                stats[key] = {'median': statistics.median(numbers), 'min': min(numbers), 'max': max(numbers)}
            saved.setdefault(case, {}).setdefault(str(clients), {})[name] = stats
assert saved == read(root / 'candidate-vs-baseline/summary.json')
assert invocations == 24 and operations == 1879306
assert saved == read(root / 'audited-decision.json')['summary']
a, b = [saved['range_scan_100']['8'][name] for name in ['baseline', 'candidate']]
gap = b['tps']['median'] - a['tps']['median']
spread = max(item['tps']['max'] - item['tps']['min'] for item in [a, b])
decision = read(root / 'audited-decision.json')
assert gap == decision['range8_median_gap'] == -51.0
assert spread == decision['maximum_full_spread'] == 101.0
assert not decision['performance_gate_passed'] and gap < 0
left, right = [read(root / f'{name}-preflight.json') for name in ['baseline', 'candidate']]
assert left['cases'] == right['cases']
assert [row for row in left['settings'] if row[0] != 'port'] == [row for row in right['settings'] if row[0] != 'port']
for case, item in left['cases'].items():
    for i, counters in enumerate(item['counters'], 1):
        expected = [0, i] if case == 'range_scan_100' else [max(0, i - 5), min(5, i)]
        assert counters == expected
assert dict(csv.reader((root / 'build/status.tsv').open(), delimiter='\t')) == {'equivalence': '0', 'release': '0'}
assert '1 passed; 0 failed' in (root / 'build/equivalence.log').read_text()
restore = read(root / 'source-restoration.json')
assert restore['production_diff_empty'] and restore['src_tree'] == restore['published_src_tree']
mismatch = read(root / 'bin-only-restored-build.json')
assert mismatch['exit'] == 0 and not mismatch['binary_identical_to_verified_baseline']
assert mismatch['target_engine_sha256'] != runtime['engines']['baseline']['sha256']
assert mismatch['differing_dependency_fingerprints'] == ['thiserror', 'pg_query', 'tokio', 'serde', 'clap']
build = read(root / 'restored-build.json')
assert build['exit'] == 0 and build['binary_identical_to_verified_baseline']
assert build['target_engine_sha256'] == runtime['engines']['baseline']['sha256']
cleanup = read(root / 'cleanup.json')
assert all(item['stopped'] and item['exit'] == 0 for item in cleanup if 'pid' in item)
assert cleanup[-1]['remove_exit'] == 0
live = read(root / 'final-live-cleanup.json')
assert live['helper_absent'] and all(item['pid_absent'] and item['connection_refused'] for item in live['engines'])
print(f'Verified {invocations} invocations, {operations} checked operations, rejection gate, source/binary restoration, and recorded cleanup')
