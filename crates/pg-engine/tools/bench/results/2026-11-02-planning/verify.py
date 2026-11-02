"""Audit the portable records without commands, servers, or omitted binaries."""
from datetime import date, timedelta
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


for line in (root / 'SHA256SUMS').read_text().splitlines():
    expected, name = line.split('  ', 1)
    assert hashlib.sha256((root / name).read_bytes()).hexdigest() == expected, name

spec = importlib.util.spec_from_file_location('paired', root / 'initial/paired_oltp.py')
paired = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paired)
# Regenerate the deterministic fixture independently, including canonical date text.
n = 200000
hasher = hashlib.md5()
owner_sum = balance_sum = 0
for i in range(1, n + 1):
    owner, balance = i % 50000, i * 7919 % 100000
    owner_sum += owner
    balance_sum += balance
    row = f'{i}:{owner}:{balance}:note {i}:{date(2024, 1, 1) + timedelta(days=i % 365)}'
    hasher.update((',' if i > 1 else '').encode() + row.encode())
fixture = f'{n}|{n * (n + 1) // 2}|{owner_sum}|{balance_sum}|{hasher.hexdigest()}'
manifest = read(root / 'manifest.json')
assert fixture == manifest['expected_fixture']
initial = read(root / 'initial/stopped-fixture-files.json')
assert len(initial) == 96
assert initial == read(root / 'continuation/stopped-fixture-files.json')
engine = manifest['engine_sha256']
client = manifest['client_sha256']
invocations = operations = 0
for relative in manifest['completed_blocks']:
    block = root / relative
    run = block.parent
    config = read(block / 'paired/config.json')
    inputs = read(run / 'inputs.json')
    hashes = inputs['hashes']
    assert engine in hashes.values() and client in hashes.values()
    for script in ['paired_oltp.py', 'wire.py']:
        expected = [digest for path, digest in hashes.items() if Path(path).name == script]
        assert expected == [hashlib.sha256((run / script).read_bytes()).hexdigest()]
    assert config['bench_sha256'] == client
    assert config['runner_sha256'] == hashlib.sha256((run / 'paired_oltp.py').read_bytes()).hexdigest()
    assert config['expected_fixture'] == fixture and config['rounds'] == 5 and config['seconds'] == 10
    assert inputs['parent_status'] == ''
    assert inputs['engine_production_tree'] == manifest['engine_production_tree']
    saved = {}
    operation_counts = {}
    settings = []
    preflights = []
    for target in config['targets']:
        name = target['name']
        runtime = read(block / f'runtime-{name}.json')
        assert runtime == config['runtime_provenance'][name]
        assert runtime['sha256'] == target['actual_executable_sha256'] == engine
        assert runtime['affinity'] == [28, 29, 30, 31]
        assert runtime['thread_affinities'] == [[28, 29, 30, 31]]
        preflight = read(block / f'{name}-preflight.json')
        preflights.append(preflight)
        settings.append([row for row in preflight['settings'] if row[0] not in {'port', 'plan_cache_mode'}])
        for case, probe in preflight['cases'].items():
            for i, counters in enumerate(probe['counters'], 1):
                expected = ([i, 0] if name == 'generic' else [0, i] if name == 'custom' or case == 'range_scan_100'
                            else [max(0, i - 5), min(5, i)])
                assert counters == expected
        for stage in ['before', 'after']:
            assert (block / f'paired/fixture-{stage}-{name}.txt').read_text().strip() == fixture
        for case, counts in config['cases'].items():
            for clients in counts:
                samples = []
                for round_no in range(6):
                    stem = block / f'paired/{round_no:02d}-{name}-{case}-{clients}'
                    values = paired.parse(stem.with_suffix('.log').read_text(), name, case, clients)
                    resource = read(stem.with_suffix('.json'))
                    assert resource['exit'] == 0
                    for kind in ['server', 'client']:
                        assert resource[kind + '_cpu_s'] > 0
                        values[kind + '_cpu_us_per_op'] = resource[kind + '_cpu_s'] * 1e6 / int(values['ops'])
                    invocations += 1
                    operations += int(values['ops'])
                    operation_counts[name] = operation_counts.get(name, 0) + int(values['ops'])
                    if round_no:
                        samples.append(values)
                stats = {'samples': len(samples)}
                for key in ['tps', 'p50_ms', 'p95_ms', 'p99_ms', 'server_cpu_us_per_op', 'client_cpu_us_per_op', 'peak_mem_mb']:
                    numbers = [float(item[key]) for item in samples]
                    stats[key] = {'median': statistics.median(numbers), 'min': min(numbers), 'max': max(numbers)}
                saved.setdefault(case, {}).setdefault(str(clients), {})[name] = stats
        if config['instrumented']:
            phase = read(block / f'{name}-pgss.json')
            raw = dict(zip(phase['columns'], phase['raw']))
            calls = int(raw['calls'])
            assert phase['includes_warmup']
            assert calls == int(raw['plans']) == phase['checked_ops'] == operation_counts[name]
            assert int(raw['rows']) == 100 * calls
            plan, inclusive = float(raw['total_plan_time_ms']), float(raw['total_exec_time_ms_inclusive'])
            assert 0 <= plan <= inclusive
            assert phase['planning_elapsed_us_per_call'] == 1000 * plan / calls
            assert phase['inclusive_elapsed_us_per_call'] == 1000 * inclusive / calls
            assert phase['remaining_elapsed_us_per_call'] == 1000 * (inclusive - plan) / calls
            assert phase['planning_fraction_of_inclusive_elapsed'] == plan / inclusive
    assert settings[0] == settings[1]
    for case in preflights[0]['cases']:
        left, right = [item['cases'][case]['plans'] for item in preflights]
        assert len(left) == len(right) == 3
        allowed = {'Index Cond', 'Plan Rows', 'Total Cost'} if case == 'range_scan_100' else {'Index Cond'}
        for a, b in zip(left, right):
            assert a['value'] == b['value']
            assert len(a['explain']) == len(b['explain']) == 1
            # Instrumented EXPLAIN also reports a mode-dependent Query Identifier.
            assert set(a['explain'][0]) <= {'Plan', 'Query Identifier'}
            assert set(b['explain'][0]) <= {'Plan', 'Query Identifier'}
            def without_differences(value):
                if isinstance(value, dict):
                    return {key: without_differences(item) for key, item in value.items() if key not in allowed}
                if isinstance(value, list):
                    return [without_differences(item) for item in value]
                return value
            assert without_differences(a['explain'][0]['Plan']) == without_differences(b['explain'][0]['Plan'])
    assert saved == read(block / 'paired/summary.json'), relative
    assert read(block / 'complete.json')['cleanly_stopped']
assert invocations == 84 and operations == 4802187
for run in ['initial', 'continuation']:
    for entry in read(root / run / 'cleanup.json'):
        if 'pid' in entry:
            assert entry['stopped'] and entry['exit'] == 0
        else:
            assert entry['remove_exit'] == 0
assert read(root / 'continuation/complete.json')['diagnostic_only']
assert 'Address already in use' in (root / 'console.log').read_text()
print(f'Verified 3 comparisons, {invocations} invocations, {operations} checked operations, fixture generation, phase arithmetic, and recorded cleanup')
