"""Verify the portable records without running servers or benchmarks."""
from pathlib import Path
import csv
import hashlib
import importlib.util
import json
import math
import re
import statistics
import sys

sys.dont_write_bytecode = True
root = Path(__file__).resolve().parent
for line in (root / 'SHA256SUMS').read_text().splitlines():
    expected, name = line.split('  ', 1)
    assert hashlib.sha256((root / name).read_bytes()).hexdigest() == expected, name

spec = importlib.util.spec_from_file_location('paired', root / 'paired_oltp.py')
paired = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paired)
comparisons = json.loads((root / 'comparisons.json').read_text())['comparisons']
audit = json.loads((root / 'completed-evidence-audit.json').read_text())
invocations = 0
for name, comparison in comparisons.items():
    directory = root / 'raw' / name
    config = comparison['config']
    assert config == json.loads((directory / 'config.json').read_text())
    assert comparison['summary'] == json.loads((directory / 'summary.json').read_text())
    assert config['runner_sha256'] == hashlib.sha256((root / 'paired_oltp.py').read_bytes()).hexdigest()
    if 'runtime_observation' in comparison:
        observation = comparison['runtime_observation']
        sidecar = directory / 'runtime-observation.json'
        if not sidecar.exists():
            sidecar = root / config['runtime_provenance']
        assert json.loads(sidecar.read_text()) == observation
        assert hashlib.sha256(sidecar.read_bytes()).hexdigest() == comparison['runtime_observation_sha256']
    count, checked = 0, 0
    samples = {}
    for target in config['targets']:
        for stage in ['before', 'after']:
            assert (directory / f"fixture-{stage}-{target['name']}.txt").read_text().strip() == config['expected_fixture']
        for case, clients_list in config['cases'].items():
            for clients in clients_list:
                group = []
                for round_no in range(config.get('rounds', 5) + 1):
                    stem = directory / f"{round_no:02d}-{target['name']}-{case}-{clients}"
                    result = paired.parse(stem.with_suffix('.log').read_text(), target['name'], case, clients)
                    resource = json.loads(stem.with_suffix('.json').read_text())
                    assert resource['exit'] == 0
                    for kind in ['server', 'client']:
                        cpu = resource[kind + '_cpu_s']
                        assert math.isfinite(cpu) and cpu > 0
                        result[kind + '_cpu_us_per_op'] = cpu * 1e6 / int(result['ops'])
                    count += 1
                    checked += int(result['checked_ops'])
                    if round_no:
                        group.append(result)
                stats = {'samples': len(group)}
                for key in ['tps', 'p50_ms', 'p95_ms', 'p99_ms', 'server_cpu_us_per_op', 'client_cpu_us_per_op', 'peak_mem_mb']:
                    numbers = [float(item[key]) for item in group]
                    stats[key] = {'median': statistics.median(numbers), 'min': min(numbers), 'max': max(numbers)}
                samples.setdefault(case, {}).setdefault(str(clients), {})[target['name']] = stats
    assert samples == comparison['summary'], name
    assert count == audit[name]['invocations']
    assert checked == audit[name]['checked_operations_including_warmup']
    invocations += count

for name in ['live-negative-results.json', 'final-live-negative-results.json']:
    records = json.loads((root / name).read_text())
    assert len(records) == 8
    assert all(record['exit'] != 0 and record['rejected'] for record in records)

def results(label):
    directory = root / 'regression' / label
    assert (directory / 'status.tsv').read_text().strip() == 'regress\t0'
    lines = (directory / 'report/results.tsv').read_text().splitlines()
    rows = list(csv.DictReader((line for line in lines if not line.startswith('#')), delimiter='\t'))
    assert len(rows) == 230 and sum(int(row['units']) for row in rows) == 51150
    assert all(row['engine_status'] == 'ok' and row['panics'] == '0' for row in rows)
    return rows

baseline, current = results('baseline'), results('current')
assert baseline == current
assert sum(int(row['match']) for row in current) == 51148
for label in ['baseline', 'current']:
    hunks = {path.name: path.read_bytes() for path in (root / 'regression' / label / 'report/hunks').glob('*.diff')}
    if label == 'baseline':
        original = hunks
    else:
        assert hunks == original
verification = json.loads((root / 'final-verification.json').read_text())
assert verification['gates'] == {'build': '0', 'test': '0', 'clippy': '0', 'pg-unit': '0'}
gate_rows = dict(csv.reader((root / 'final-gates/status.tsv').open(), delimiter='\t'))
assert gate_rows == verification['gates']
tests = re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored', (root / 'final-gates/test.log').read_text())
assert len(tests) == 60 and all(item[0] == 'ok' and item[2] == '0' for item in tests)
assert [sum(int(item[i]) for item in tests) for i in range(1, 4)] == [430, 0, 18]
assert verification['crate_tests'] == {'passed': 430, 'failed': 0, 'ignored': 18, 'completed_targets': 60}
cleanup = json.loads((root / 'final-cleanup.json').read_text())
assert not any(cleanup['listeners'].values())
assert not any(item['running'] for item in cleanup['containers'])
print(f'Verified {len(comparisons)} comparisons, {invocations} checked invocations, and unchanged 51148/51150 regression units')
