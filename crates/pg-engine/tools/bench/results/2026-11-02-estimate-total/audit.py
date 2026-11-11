"""Recompute the paired acceptance gate from saved logs only."""
from pathlib import Path
import importlib.util
import json
import statistics
import sys

sys.dont_write_bytecode = True
root = Path(__file__).resolve().parent
runner = Path('/home/meszmate/worktrees/roll/t3-d4020b98/crates/pg-engine/tools/bench/paired_oltp.py')
spec = importlib.util.spec_from_file_location('paired', runner)
paired = importlib.util.module_from_spec(spec); spec.loader.exec_module(paired)
config = json.loads((root / 'candidate-vs-baseline/config.json').read_text())
summary = {}
invocations = operations = 0
for target in config['targets']:
    name = target['name']
    for stage in ['before', 'after']:
        assert (root / f'candidate-vs-baseline/fixture-{stage}-{name}.txt').read_text().strip() == config['expected_fixture']
    for case, counts in config['cases'].items():
        for clients in counts:
            samples = []
            for round_no in range(6):
                stem = root / f'candidate-vs-baseline/{round_no:02d}-{name}-{case}-{clients}'
                values = paired.parse(stem.with_suffix('.log').read_text(), name, case, clients)
                resource = json.loads(stem.with_suffix('.json').read_text())
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
            summary.setdefault(case, {}).setdefault(str(clients), {})[name] = stats
assert summary == json.loads((root / 'candidate-vs-baseline/summary.json').read_text())
a, b = [summary['range_scan_100']['8'][name] for name in ['baseline', 'candidate']]
gap = b['tps']['median'] - a['tps']['median']
spread = max(item['tps']['max'] - item['tps']['min'] for item in [a, b])
point_a, point_b = [summary['point_select']['1'][name] for name in ['baseline', 'candidate']]
point_gap = point_a['tps']['median'] - point_b['tps']['median']
point_spread = max(item['tps']['max'] - item['tps']['min'] for item in [point_a, point_b])
accepted = gap > spread and b['server_cpu_us_per_op']['median'] < a['server_cpu_us_per_op']['median'] and point_gap <= point_spread
result = {'invocations': invocations, 'operations': operations, 'range8_median_gain_percent': (b['tps']['median'] / a['tps']['median'] - 1) * 100, 'range8_median_gap': gap, 'maximum_full_spread': spread, 'point1_regression_within_full_spread': point_gap <= point_spread, 'performance_gate_passed': accepted, 'limits': 'Performance gate only; acceptance still requires full source and regression gates. Failure does not prove zero benefit.', 'summary': summary}
(root / 'audited-decision.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps({key: value for key, value in result.items() if key != 'summary'}, indent=2))
