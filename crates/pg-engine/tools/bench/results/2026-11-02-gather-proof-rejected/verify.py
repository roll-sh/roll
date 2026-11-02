"""Verify saved evidence without starting workloads or services."""
import hashlib
import importlib.util
import json
import statistics
from pathlib import Path
import sys

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent


def check(ok, message):
    if not ok:
        raise RuntimeError(message)


def read(name):
    return json.loads((ROOT / name).read_text())


def verify():
    entries = {}
    for line in (ROOT / 'SHA256SUMS').read_text().splitlines():
        digest, name = line.split('  ', 1)
        path = Path(name)
        check(not path.is_absolute() and '..' not in path.parts, 'unsafe manifest path')
        check(name not in entries, 'duplicate manifest path')
        check(hashlib.sha256((ROOT / path).read_bytes()).hexdigest() == digest, name)
        entries[name] = digest
    actual = {str(p.relative_to(ROOT)) for p in ROOT.rglob('*') if p.is_file() and p.name != 'SHA256SUMS'}
    check(actual == entries.keys(), 'manifest inventory mismatch')
    spec = importlib.util.spec_from_file_location('saved_evaluation', ROOT / 'evaluate.py')
    evaluation = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(evaluation)
    evaluation.self_test()
    primary = evaluation.evaluate(ROOT / 'proof-primary-p1')
    fallback = evaluation.evaluate(ROOT / 'proof-declined-p1', True)
    check(primary == read('proof-primary-statistics.json'), 'primary recomputation mismatch')
    check(fallback == read('proof-declined-statistics.json'), 'fallback recomputation mismatch')
    check(primary['performance_rule_passes'] and not fallback['performance_rule_passes'], 'gate verdict')
    check(read('performance-decision.json')['action'] == 'reject', 'candidate must remain rejected')
    source = read('candidate-source/manifest.json')
    binary = read('candidate-bin/manifest.json')
    check(binary['source_sha256'] == source, 'source identity')
    for name, digest in source.items():
        check(hashlib.sha256((ROOT / 'candidate-source' / name).read_bytes()).hexdigest() == digest, name)
    frame = read('proof-frame.json')
    for name in ['evaluate.py', 'measure.py', 'audit_pairs.py', 'run_measurements.py', 'run_source_gates.py', 'run_regress.py']:
        matches = [h for p, h in frame['files_sha256'].items() if p.endswith('/' + name)]
        check(matches == [hashlib.sha256((ROOT / name).read_bytes()).hexdigest()], 'frozen tool ' + name)
    total = count = 0
    for phase in ['proof-primary-p1', 'proof-declined-p1']:
        runtime = read(phase + '/runtime.json')
        check(runtime['source_sha256'] == source, 'runtime source')
        check(runtime['engines']['candidate']['sha256'] == binary['binary_sha256'] == frame['candidate_sha256'], 'candidate ELF identity')
        check(runtime['engines']['baseline']['sha256'] == frame['baseline_sha256'], 'baseline identity')
        logs = sorted((ROOT / phase / 'candidate-vs-baseline').glob('*.log'))
        for path in logs:
            lines = [line for line in path.read_text().splitlines() if line.startswith('RESULT\t')]
            check(len(lines) == 1, 'result count')
            values = dict(item.split('=', 1) for item in lines[0].split('\t')[4:])
            check(int(values['checked_ops']) == int(values['ops']) > 0 and values['errors'] == '0', 'work/error count')
            total += int(values['ops'])
            count += 1
    check((count, total) == (76, 11168976), 'combined count')
    gather = read('proof-declined-p1/real-gather-control.json')
    samples = gather['samples']
    check(len(samples) == 12, 'actual Gather sample inventory')
    check(sum(s['checked_queries'] for s in samples) == 1655, 'actual Gather queries')
    check(all(s['checked_rows'] == s['checked_queries'] * 4000 for s in samples), 'actual Gather rows')
    ratios = {'cpu': [], 'latency': []}
    for i in range(1, 6):
        pair = {s['name']: s for s in samples if s['round'] == i}
        for metric, field in [('cpu', 'server_cpu_s'), ('latency', 'elapsed_s')]:
            b, c = pair['baseline'], pair['candidate']
            ratios[metric].append((c[field] / c['checked_queries']) / (b[field] / b['checked_queries']))
    check(ratios['cpu'] == gather['cpu_ratios'] and ratios['latency'] == gather['latency_ratios'], 'actual Gather ratios')
    check(all(statistics.median(v) <= 1.05 for v in ratios.values()) and gather['passes'], 'actual Gather control gate')
    monitor = read('monitor-correctness/complete.json')
    check(monitor['source_fixture_unchanged'] and not monitor['benchmark_executed'], 'monitor correctness phase')
    live = read('final-live-cleanup.json')
    check(len(live['phases']) == 3 and len(live['ports']) == 6, 'cleanup inventory')
    check(all(p['bindable'] for p in live['ports']), 'recorded ports')
    for phase in live['phases']:
        check(len(phase['checks']) == 3 and all(c['absent'] for c in phase['checks']), 'recorded processes/helpers')
        check(len(phase['removed_private_copies']) == 2, 'private data cleanup')
    restored = read('rejection-restore.json')
    check(restored['diff'] == '' and restored['binary']['sha256'] == frame['baseline_sha256'], 'baseline restore')
    return {'files': len(entries), 'decision': 'rejected and reverted', 'checked_operations': total,
            'primary_pass': True, 'fallback_pass': False, 'fresh_full_source_gates': False}


if __name__ == '__main__':
    print(json.dumps(verify(), indent=2))
