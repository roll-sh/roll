"""Locked prospective P1 paired statistics, using exhaustive signed-rank enumeration."""

def check(condition, message):
    if not condition:
        raise RuntimeError(message)

import argparse
import json
import math
from pathlib import Path
import statistics


def exact_signed_rank(log_ratios):
    values = [v for v in log_ratios if v != 0]
    if not values:
        return 1.0
    ordered = sorted(range(len(values)), key=lambda i: abs(values[i]))
    ranks = [0] * len(values)
    start = 0
    while start < len(values):
        end = start + 1
        while end < len(values) and abs(values[ordered[end]]) == abs(values[ordered[start]]):
            end += 1
        for i in ordered[start:end]:
            ranks[i] = start + 1 + end  # twice the average rank
        start = end
    observed = sum(rank for rank, v in zip(ranks, values) if v > 0)
    totals = [0]
    for rank in ranks:
        totals += [total + rank for total in totals]
    return sum(total >= observed for total in totals) / len(totals)


def hodges_lehmann(values):
    return statistics.median((a + b) / 2 for i, a in enumerate(values) for b in values[i:])


def self_test():
    check(exact_signed_rank([1] * 12) == 1 / 4096, 'Validation failed in evaluate.py, original line 35')
    check(exact_signed_rank([-1] * 12) == 1, 'Validation failed in evaluate.py, original line 36')
    check(exact_signed_rank([0] * 12) == 1, 'Validation failed in evaluate.py, original line 37')
    check(exact_signed_rank([1, 1, -1]) == .5, 'Validation failed in evaluate.py, original line 38')
    check(exact_signed_rank([1, -2, 3]) == .375, 'Validation failed in evaluate.py, original line 39')
    check(hodges_lehmann([1, 2, 3]) == 2, 'Validation failed in evaluate.py, original line 40')
    check(hodges_lehmann([math.log(1.04)] * 12) == math.log(1.04), 'Validation failed in evaluate.py, original line 41')


def read_result(path):
    lines = [l.split('\t') for l in path.read_text().splitlines() if l.startswith('RESULT\t')]
    check(len(lines) == 1, 'Validation failed in evaluate.py, original line 46')
    fields = [x.split('=', 1) for x in lines[0][4:]]
    values = dict(fields)
    check(len(values) == len(fields), 'Validation failed in evaluate.py, original line 49')
    check(values['errors'] == '0', 'Validation failed in evaluate.py, original line 50')
    check(int(values['checked_ops']) == int(values['ops']) > 0, 'Validation failed in evaluate.py, original line 51')
    record = json.loads(path.with_suffix('.json').read_text())
    check(record['exit'] == 0, 'Validation failed in evaluate.py, original line 53')
    return float(values['tps']), record['server_cpu_s'] * 1e6 / int(values['ops'])


def evaluate(root, decline=False):
    out = root / 'candidate-vs-baseline'
    config = json.loads((out / 'config.json').read_text())
    rounds = 5 if decline else 12
    check(config['rounds'] == rounds and config['seconds'] == (15 if decline else 20), 'Validation failed in evaluate.py, original line 61')
    check(config['cases'] == {'range_scan_100': [8], 'point_select': [1]}, 'Validation failed in evaluate.py, original line 62')
    check(json.loads((root / 'complete.json').read_text())['source_fixture_unchanged'], 'Validation failed in evaluate.py, original line 63')
    runtime = json.loads((root / 'runtime.json').read_text())
    check(runtime['declined_proof_phase'] == decline, 'Validation failed in evaluate.py, original line 65')
    cleanup = json.loads((root / 'cleanup.json').read_text())
    check(len(cleanup) == 3, 'Validation failed in evaluate.py, original line 67')
    check(all(row.get('stopped') and row.get('exit') == 0 for row in cleanup if 'pid' in row), 'Validation failed in evaluate.py, original line 68')
    check(all(row.get('remove_exit') == 0 for row in cleanup if 'helper_id' in row), 'Validation failed in evaluate.py, original line 69')
    observation = json.loads((root / 'host-observation.json').read_text())
    check(observation['stopped'] and not observation['errors'], 'Validation failed in evaluate.py, original line 71')
    actual_control_ok = not decline or json.loads((root / 'real-gather-control.json').read_text())['passes']
    paired = {}
    utilization = []
    for case, clients in [('range_scan_100', 8), ('point_select', 1)]:
        metrics = {key: [] for key in ['tps', 'cpu', 'p99_ms', 'peak_mem_mb']}
        for i in range(1, rounds + 1):
            sides = []
            for name in ['baseline', 'candidate']:
                path = out / f'{i:02}-{name}-{case}-{clients}.log'
                tps, cpu = read_result(path)
                line = next(l for l in path.read_text().splitlines() if l.startswith('RESULT\t'))
                v = dict(x.split('=', 1) for x in line.split('\t')[4:])
                record = json.loads(path.with_suffix('.json').read_text())
                elapsed = float(v['elapsed_s'])
                utilization.append({'round': i, 'name': name, 'case': case,
                    'client_cores': record['client_cpu_s'] / elapsed,
                    'fraction_of_four_client_cpus': record['client_cpu_s'] / elapsed / 4,
                    'server_cores': record['server_cpu_s'] / elapsed})
                sides.append({'tps': tps, 'cpu': cpu, 'p99_ms': float(v['p99_ms']), 'peak_mem_mb': float(v['peak_mem_mb'])})
            for key in metrics:
                metrics[key].append(sides[1][key] / sides[0][key])
        paired[case] = {'ratios': metrics, 'median_ratios': {key: statistics.median(v) for key, v in metrics.items()}}
    rr = paired['range_scan_100']['ratios']
    log_ratios = [math.log(v) for v in rr['tps']]
    p = exact_signed_rank(log_ratios)
    hl = hodges_lehmann(log_ratios)
    range_m = paired['range_scan_100']['median_ratios']
    point_m = paired['point_select']['median_ratios']
    guards = all(v['median_ratios']['peak_mem_mb'] <= 1.05 and v['median_ratios']['p99_ms'] <= 1.10 for v in paired.values())
    passed = (range_m['tps'] >= .97 and point_m['tps'] >= .97 if decline else p <= .01 and hl >= math.log(1.02) and range_m['cpu'] < 1 and point_m['tps'] >= .98)
    range_utilization = [v for v in utilization if v['case'] == 'range_scan_100']
    limiter_ok = max(v['fraction_of_four_client_cpus'] for v in range_utilization) < .8 and statistics.median(v['server_cores'] for v in range_utilization) >= 3
    return {'utilization_per_sample': utilization, 'server_limiter_check_passes': limiter_ok, 'phase': 'declined-proof' if decline else 'primary', 'paired': paired,
            'one_sided_exact_signed_rank_p': p, 'hodges_lehmann_gain_pct': 100 * math.expm1(hl),
            'memory_and_tail_guards_pass': guards, 'performance_rule_passes': passed and guards and limiter_ok and actual_control_ok, 'actual_gather_control_passes': actual_control_ok if decline else None,
            'limit': 'Source gates, accounting/identity audit, and actual-Gather performance remain separate prerequisites. CPU/op under saturation is not independent of throughput.'}

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('run', type=Path, nargs='?')
    parser.add_argument('--decline', action='store_true')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
        print('Seven analytical statistics checks passed.')
    else:
        check(args.run is not None, 'Validation failed in evaluate.py, original line 119')
        print(json.dumps(evaluate(args.run, args.decline), indent=2))
