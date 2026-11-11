"""Audit captured records without running the original commands or servers."""
from pathlib import Path
import csv
import hashlib
import importlib.util
import json
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
expected_fixture = json.loads((root / 'independent-fixture.json').read_text())['expected']
client = json.loads((root / 'client-binary.json').read_text())
candidate = json.loads((root / 'candidate-binary.json').read_text())
total = 0
for name in ['candidate-vs-baseline', 'long-control-vs-baseline']:
    directory = root / name
    config = json.loads((directory / 'config.json').read_text())
    assert config['expected_fixture'] == expected_fixture
    assert config['bench_sha256'] == client['sha256']
    assert config['runner_sha256'] == hashlib.sha256((root / 'paired_oltp.py').read_bytes()).hexdigest()
    runtime = json.loads((root / config['runtime_provenance']).read_text())
    assert runtime['engines']['candidate']['sha256'] == candidate['binary_sha256']
    assert runtime['engines']['baseline']['sha256'] == json.loads((root / 'baseline-binary.json').read_text())['binary_sha256']
    saved = {}
    for target in config['targets']:
        assert target['actual_executable_sha256'] == runtime['engines'][target['name']]['sha256']
        for stage in ['before', 'after']:
            assert (directory / f"fixture-{stage}-{target['name']}.txt").read_text().strip() == expected_fixture
        for case, client_counts in config['cases'].items():
            for clients in client_counts:
                samples = []
                for round_no in range(6):
                    stem = directory / f"{round_no:02d}-{target['name']}-{case}-{clients}"
                    values = paired.parse(stem.with_suffix('.log').read_text(), target['name'], case, clients)
                    resource = json.loads(stem.with_suffix('.json').read_text())
                    assert resource['exit'] == 0
                    for kind in ['server', 'client']:
                        assert resource[kind + '_cpu_s'] > 0
                        values[kind + '_cpu_us_per_op'] = resource[kind + '_cpu_s'] * 1e6 / int(values['ops'])
                    total += 1
                    if round_no:
                        samples.append(values)
                stats = {'samples': len(samples)}
                for key in ['tps', 'p50_ms', 'p95_ms', 'p99_ms', 'server_cpu_us_per_op', 'client_cpu_us_per_op', 'peak_mem_mb']:
                    numbers = [float(item[key]) for item in samples]
                    stats[key] = {'median': statistics.median(numbers), 'min': min(numbers), 'max': max(numbers)}
                saved.setdefault(case, {}).setdefault(str(clients), {})[target['name']] = stats
    assert saved == json.loads((directory / 'summary.json').read_text()), name
assert total == 84
assert (root / 'baseline-plans.log').read_bytes() == (root / 'candidate-plans.log').read_bytes()
probe = json.loads((root / 'plan-choice-results.json').read_text())
for item in probe['results']:
    for n, pair in enumerate(item['counters']):
        if item['mode'] == 'force_generic_plan':
            expected = [n, 0]
        elif item['mode'] == 'force_custom_plan' or item['case'] == 'range_scan_100':
            expected = [0, n]
        else:
            expected = [max(0, n - 5), min(5, n)]
        assert pair == expected
assert dict(csv.reader((root / 'build/status.tsv').open(), delimiter='\t')) == {'equivalence': '0', 'release': '0'}
assert '1 passed; 0 failed' in (root / 'build/equivalence.log').read_text()
restored = json.loads((root / 'source-restoration.json').read_text())
assert restored['production_diff_empty'] and restored['src_tree'] == restored['published_src_tree']
build = json.loads((root / 'restored-build.json').read_text())
assert build['exit'] == 0 and build['binary_identical_to_verified_baseline']
assert build['target_engine_sha256'] == build['verified_baseline_engine_sha256']
cleanup = json.loads((root / 'cleanup.json').read_text())
assert not any(cleanup['listeners'].values()) and cleanup['postgres_helper_stopped']
assert all(item['stopped'] for item in cleanup['engines'])
print('Verified 84 checked invocations, exact summaries/plans, source restoration, and stopped services')
