"""Locked prospective P1 paired statistics, using exhaustive signed-rank enumeration."""
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
    assert exact_signed_rank([1] * 12) == 1 / 4096
    assert exact_signed_rank([-1] * 12) == 1
    assert exact_signed_rank([0] * 12) == 1
    assert exact_signed_rank([1, 1, -1]) == .5
    assert exact_signed_rank([1, -2, 3]) == .375
    assert hodges_lehmann([1, 2, 3]) == 2
    assert hodges_lehmann([math.log(1.04)] * 12) == math.log(1.04)


def read_result(path):
    lines = [l.split('\t') for l in path.read_text().splitlines() if l.startswith('RESULT\t')]
    assert len(lines) == 1
    fields = [x.split('=', 1) for x in lines[0][4:]]
    values = dict(fields)
    assert len(values) == len(fields)
    assert values['errors'] == '0'
    assert int(values['checked_ops']) == int(values['ops']) > 0
    record = json.loads(path.with_suffix('.json').read_text())
    assert record['exit'] == 0
    return float(values['tps']), record['server_cpu_s'] * 1e6 / int(values['ops'])


def evaluate(root):
    out = root / 'candidate-vs-baseline'
    config = json.loads((out / 'config.json').read_text())
    assert config['rounds'] == 12 and config['seconds'] == 30
    assert config['cases'] == {'range_scan_100': [8], 'point_select': [1]}
    assert json.loads((root / 'complete.json').read_text())['source_fixture_unchanged']
    points, logs, cpus = [], [], []
    for i in range(1, 13):
        def pair(case, clients):
            return [read_result(out / f'{i:02}-{name}-{case}-{clients}.log')
                    for name in ['baseline', 'candidate']]
        b, c = pair('range_scan_100', 8)
        logs.append(math.log(c[0] / b[0]))
        cpus.append(c[1] / b[1])
        b, c = pair('point_select', 1)
        points.append(c[0] / b[0])
    pvalue = exact_signed_rank(logs)
    hl = hodges_lehmann(logs)
    cpu, point = statistics.median(cpus), statistics.median(points)
    return {'range_log_ratios': logs, 'range_ratios': [math.exp(v) for v in logs],
            'one_sided_exact_signed_rank_p': pvalue,
            'hodges_lehmann_gain_pct': 100 * math.expm1(hl),
            'median_range_cpu_ratio': cpu, 'point_ratios': points,
            'median_point_throughput_ratio': point,
            'p1_statistical_rule_passes': pvalue <= .01 and hl >= math.log(1.02) and cpu < 1 and point >= .98,
            'limit': 'The full accounting/identity audit and source/regression gates remain separate requirements.'}


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('run', type=Path, nargs='?')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
        print('Seven analytical statistics checks passed.')
    else:
        assert args.run is not None
        print(json.dumps(evaluate(args.run), indent=2))
