"""Recompute checked paired measurements without running either server."""

def check(condition, message):
    if not condition:
        raise RuntimeError(message)

import argparse
import json
from pathlib import Path
import statistics


def audit(root):
    out = root / 'candidate-vs-baseline'
    config = json.loads((out / 'config.json').read_text())
    summary = json.loads((out / 'summary.json').read_text())
    preflights = [json.loads((root / (name + '-preflight.json')).read_text())
                  for name in ['baseline', 'candidate']]
    check(preflights[0]['cases'] == preflights[1]['cases'], 'Validation failed in audit_pairs.py, original line 14')
    settings = [[r for r in p['settings'] if r[0] != 'port'] for p in preflights]
    check(settings[0] == settings[1], 'Validation failed in audit_pairs.py, original line 16')
    check(json.loads((root / 'complete.json').read_text())['source_fixture_unchanged'], 'Validation failed in audit_pairs.py, original line 17')
    cleanup = json.loads((root / 'cleanup.json').read_text())
    check(len(cleanup) == 3, 'Validation failed in audit_pairs.py, original line 19')
    check(all(r['exit'] == 0 and r['stopped'] for r in cleanup if 'pid' in r), 'Validation failed in audit_pairs.py, original line 20')
    check(all(r['remove_exit'] == 0 for r in cleanup if 'helper_id' in r), 'Validation failed in audit_pairs.py, original line 21')
    saved, invocations, operations = {}, 0, 0
    for path in sorted(out.glob('*.log')):
        record = json.loads(path.with_suffix('.json').read_text())
        check(record['exit'] == 0, 'Validation failed in audit_pairs.py, original line 25')
        command = record['command']
        check(int(command[command.index('--secs') + 1]) == config['seconds'], 'Validation failed in audit_pairs.py, original line 27')
        lines = [l.split('\t') for l in path.read_text().splitlines()
                 if l.startswith('RESULT\t')]
        check(len(lines) == 1, 'Validation failed in audit_pairs.py, original line 30')
        line = lines[0]
        name, case = line[1], line[3]
        fields = [f.split('=', 1) for f in line[4:]]
        v = dict(fields)
        check(len(v) == len(fields), 'Validation failed in audit_pairs.py, original line 35')
        check(v['errors'] == '0' and 'first_error' not in v, 'Validation failed in audit_pairs.py, original line 36')
        check(v['validation_version'] == 'oltp-values-v1', 'Validation failed in audit_pairs.py, original line 37')
        check(int(v['checked_ops']) == int(v['ops']) > 0, 'Validation failed in audit_pairs.py, original line 38')
        check(abs(float(v['tps']) - int(v['ops']) / float(v['elapsed_s'])) <= .51, 'Validation failed in audit_pairs.py, original line 39')
        invocations += 1
        operations += int(v['ops'])
        v['server_cpu_us_per_op'] = record['server_cpu_s'] * 1e6 / int(v['ops'])
        v['client_cpu_us_per_op'] = record['client_cpu_s'] * 1e6 / int(v['ops'])
        v['server_cores'] = record['server_cpu_s'] / float(v['elapsed_s'])
        v['client_cores'] = record['client_cpu_s'] / float(v['elapsed_s'])
        if int(path.name.split('-')[0]):
            saved.setdefault((case, v['clients'], name), []).append(v)
    check(invocations == 2 * len(config['cases']) * (config['rounds'] + 1), 'Validation failed in audit_pairs.py, original line 48')
    for (case, clients, name), samples in saved.items():
        check(len(samples) == config['rounds'], 'Validation failed in audit_pairs.py, original line 50')
        for key, reported in summary[case][clients][name].items():
            if key == 'samples':
                check(reported == len(samples), 'Validation failed in audit_pairs.py, original line 53')
            else:
                values = [float(v[key]) for v in samples]
                check(reported == dict(median=statistics.median(values),
                                        min=min(values), max=max(values)), 'Validation failed in audit_pairs.py, original line 56')
    def compare(case, clients):
        sides = summary[case][clients]
        a, b = sides['baseline'], sides['candidate']
        gap = b['tps']['median'] - a['tps']['median']
        spread = max(v['tps']['max'] - v['tps']['min'] for v in sides.values())
        return {'baseline_tps': a['tps'], 'candidate_tps': b['tps'],
                'throughput_gain_pct': 100 * gap / a['tps']['median'],
                'gap': gap, 'largest_full_spread': spread,
                'baseline_cpu_us_per_op': a['server_cpu_us_per_op'],
                'candidate_cpu_us_per_op': b['server_cpu_us_per_op'],
                'cpu_change_pct': 100 * (b['server_cpu_us_per_op']['median'] /
                                        a['server_cpu_us_per_op']['median'] - 1)}
    ranges = compare('range_scan_100', '8')
    points = compare('point_select', '1') if 'point_select' in summary else None
    passed = (ranges['gap'] > ranges['largest_full_spread'] and
              ranges['cpu_change_pct'] < 0 and
              (points is None or points['gap'] >= -points['largest_full_spread']))
    cores = {':'.join(k): {metric: statistics.median(float(v[metric]) for v in vs)
                           for metric in ['server_cores', 'client_cores']}
             for k, vs in saved.items()}
    return {'run': str(root), 'rounds': config['rounds'],
            'seconds': config['seconds'], 'invocations_including_warmups': invocations,
            'checked_operations_including_warmups': operations, 'errors': 0,
            'range8': ranges, 'point1': points, 'cores': cores,
            'descriptive_unpaired_spread_check': passed,
            'point_control_in_this_run': points is not None,
            'limit': 'This audits saved measurements, not source correctness, host isolation, or product acceptance.'}


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('runs', type=Path, nargs='+')
    args = parser.parse_args()
    print(json.dumps([audit(p) for p in args.runs], indent=2))
