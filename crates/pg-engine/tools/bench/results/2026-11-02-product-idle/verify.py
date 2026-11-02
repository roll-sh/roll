"""Read-only portable audit of the rejected idle trial and baseline migrations."""
from datetime import date, timedelta
import csv
import hashlib
import json
import math
from pathlib import Path
import re
import statistics
import xml.etree.ElementTree as ET

root = Path(__file__).resolve().parent

def read(name):
    return json.loads((root / name).read_text())

def stats(values):
    return {'median': statistics.median(values), 'min': min(values), 'max': max(values)}

inventory = {}
for line in (root / 'SHA256SUMS').read_text().splitlines():
    digest, name = line.split('  ', 1)
    assert name not in inventory and not Path(name).is_absolute() and '..' not in Path(name).parts, name
    inventory[name] = digest
    assert hashlib.sha256((root / name).read_bytes()).hexdigest() == digest, name
assert set(inventory) == {p.relative_to(root).as_posix() for p in root.rglob('*')
                          if p.is_file() and p != root / 'SHA256SUMS'}

baseline = '5cfc79043a50d0ac228bdac3c66deb5fcf0dd34d73add389bf2773237e51ffd1'
candidate = '9c45c0677b86e9b415c34fbaf47e5358b829816248bbc2ecb0cf9fc750b14aa3'
assert read('candidate-bin/manifest.json')['binary_sha256'] == candidate
fingerprints = read('matched-dependencies.json')
assert fingerprints['candidate_fingerprint']['deps'] == fingerprints['baseline_fingerprint']['deps']
assert fingerprints['dependency_fingerprints_equal']
restoration = read('source-restoration.json')
assert restoration['production_diff_empty']
assert restoration['src_tree'] == restoration['baseline_src_tree']

# Recompute all connected-idle summaries from the raw snapshots.
idle_config = read('candidate-vs-baseline-idle/observations/config.json')
assert idle_config['rounds'] == 5 and idle_config['seconds'] == 10
assert idle_config['sessions'] == 64 and idle_config['settle_seconds'] == 5
assert idle_config['phases'] == ['connected'] and idle_config['server_cpus'] == [28, 29, 30, 31]
idle_summary = read('candidate-vs-baseline-idle/observations/summary.json')['connected']
idle_identity = read('candidate-vs-baseline-idle/observations/identity.json')
assert idle_identity['runner_sha256'] == hashlib.sha256((root / 'idle.py').read_bytes()).hexdigest()
assert idle_identity['wire_sha256'] == hashlib.sha256((root / 'clients/wire.py').read_bytes()).hexdigest()
connection_rates = {}
for name in ['baseline', 'candidate', 'postgres']:
    settings = read(f'candidate-vs-baseline-idle/observations/{name}-settings.json')
    assert settings['limits'] == {'memory.max': '536870912', 'memory.swap.max': '0',
                                  'cpu.max': '400000 100000', 'cpu.weight': '100'}
    records = [read(f'candidate-vs-baseline-idle/observations/connected-{i:02d}-{name}.json')
               for i in range(6)]
    rates = []
    for i, r in enumerate(records):
        assert r['warmup'] == (i == 0) and r['round'] == i
        assert r['target'] == name and r['connections_held'] == 64
        b, a = r['before'], r['after']
        elapsed = (a['monotonic_ns'] - b['monotonic_ns']) / 1e9
        cpu = a['cpu']['usage_usec'] - b['cpu']['usage_usec']
        assert r['elapsed_s'] == elapsed >= 10 and r['cpu_us'] == cpu >= 0
        assert r['cpu_us_per_s'] == cpu / elapsed
        assert b['threads'].keys() == a['threads'].keys()
        switches = connection_switches = 0
        for tid, old in b['threads'].items():
            new = a['threads'][tid]
            assert new['start_ticks'] == old['start_ticks']
            delta = new['voluntary'] - old['voluntary']
            assert delta >= 0
            switches += delta
            if old['name'] == 'roll-conn':
                connection_switches += delta
        assert r['thread_ids_unchanged'] and r['added_threads'] == r['removed_threads'] == 0
        assert r['voluntary_switches_surviving_threads_per_s'] == switches / elapsed
        assert r['memory_current_mib'] == a['memory_current'] / 2**20
        assert r['memory_anon_shmem_mib'] == (a['memory']['anon'] + a['memory']['shmem']) / 2**20
        assert a['memory_swap_current'] == b['memory_swap_current'] == 0
        for key in ['high', 'max', 'oom', 'oom_kill']:
            assert a['memory_events'][key] == b['memory_events'][key]
        for key in ['nr_throttled', 'throttled_usec']:
            assert a['cpu'].get(key, 0) == b['cpu'].get(key, 0)
        if i:
            rates.append(connection_switches / elapsed)
    connection_rates[name] = stats(rates)
    measured = records[1:]
    saved = idle_summary[name]
    assert saved['time_weighted_cpu_us_per_s'] == sum(r['cpu_us'] for r in measured) / sum(r['elapsed_s'] for r in measured)
    assert saved['switch_claim_valid_without_churn'] and saved['thread_churn_windows'] == 0
    for key in ['cpu_us_per_s', 'voluntary_switches_surviving_threads_per_s',
                'memory_anon_shmem_mib', 'memory_current_mib']:
        assert saved[key] == stats([r[key] for r in measured])
    if name != 'postgres':
        assert saved['block_io_totals'] is None
        assert all(r['before']['io_stat'] is None and r['after']['io_stat'] is None for r in measured)
assert connection_rates['baseline']['min'] > 31
assert connection_rates['candidate']['max'] == 0
assert read('candidate-vs-baseline-idle/observations/complete.json') == {'checked_fixture_queries': 2502, 'errors': 0}
protocol = read('candidate-vs-baseline-idle/protocol-results.json')
assert set(protocol) == {'baseline', 'candidate', 'postgres'}
assert all(v == protocol['baseline'] for v in protocol.values())
for v in protocol.values():
    assert v['negative_fixture_rejected']
    for key, code in [('terminate', '57P01'), ('idle_session_timeout', '57P05'),
                      ('idle_in_transaction_session_timeout', '25P03'), ('transaction_timeout', '25P04')]:
        assert "S='FATAL'" in v[key] and f"C='{code}'" in v[key]
    assert 'D 00000001' in v['cancel_then_extended'] and "D '1'" in v['fragmented_query']

# Generate the expected active fixture independently of SQL and the benchmark.
h = hashlib.md5()
owners = balances = 0
for i in range(1, 200001):
    owner, balance = i % 50000, i * 7919 % 100000
    owners += owner
    balances += balance
    h.update(((',' if i > 1 else '') + f'{i}:{owner}:{balance}:note {i}:{date(2024, 1, 1) + timedelta(days=i % 365)}').encode())
fixture = f'200000|20000100000|{owners}|{balances}|{h.hexdigest()}'
counts = {}
for run, seconds, expected_invocations in [('active-control', 10, 24), ('active-control-r2', 20, 12)]:
    config = read(f'{run}/candidate-vs-baseline/config.json')
    assert config['expected_fixture'] == fixture
    assert config['rounds'] == 5 and config['seconds'] == seconds
    assert config['validation_version'] == 'oltp-values-v1'
    assert config['bench_sha256'] == '9f5b02222402fd5b09772088ce704054299339bb80723f1ef15fd915fb0b8e28'
    assert config['runner_sha256'] == hashlib.sha256((root / 'paired_oltp.py').read_bytes()).hexdigest()
    invocations = operations = 0
    saved = {}
    runtime = read(f'{run}/runtime.json')
    for target in config['targets']:
        name = target['name']
        identity = runtime['engines'][name]
        assert identity['sha256'] == target['actual_executable_sha256'] == {'baseline': baseline, 'candidate': candidate}[name]
        assert identity['affinity'] == [28, 29, 30, 31]
        assert identity['thread_affinities'] == [[28, 29, 30, 31]]
        for stage in ['before', 'after']:
            assert (root / f'{run}/candidate-vs-baseline/fixture-{stage}-{name}.txt').read_text().strip() == fixture
        for case, clients_list in config['cases'].items():
            for clients in clients_list:
                measured = []
                for i in range(6):
                    stem = root / f'{run}/candidate-vs-baseline/{i:02d}-{name}-{case}-{clients}'
                    lines = [s.split('\t') for s in stem.with_suffix('.log').read_text().splitlines() if s.startswith('RESULT\t')]
                    assert len(lines) == 1 and lines[0][1:4] == [name, 'oltp', case]
                    fields = [s.split('=', 1) for s in lines[0][4:]]
                    v = dict(fields)
                    assert len(v) == len(fields) and v['errors'] == '0' and 'first_error' not in v
                    assert int(v['clients']) == clients
                    assert v['validation_version'] == 'oltp-values-v1'
                    ops, elapsed = int(v['ops']), float(v['elapsed_s'])
                    assert int(v['checked_ops']) == ops > 0 and elapsed >= seconds
                    assert abs(float(v['tps']) - ops / elapsed) <= .51
                    assert float(v['p50_ms']) <= float(v['p95_ms']) <= float(v['p99_ms'])
                    resource = json.loads(stem.with_suffix('.json').read_text())
                    assert resource['exit'] == 0
                    for kind in ['server', 'client']:
                        assert resource[kind + '_cpu_s'] > 0
                        v[kind + '_cpu_us_per_op'] = resource[kind + '_cpu_s'] * 1e6 / ops
                    invocations += 1
                    operations += ops
                    if i:
                        measured.append(v)
                summary = {'samples': 5}
                for key in ['tps', 'p50_ms', 'p95_ms', 'p99_ms', 'server_cpu_us_per_op', 'client_cpu_us_per_op', 'peak_mem_mb']:
                    values = [float(v[key]) for v in measured]
                    assert all(math.isfinite(v) and v > 0 for v in values)
                    summary[key] = stats(values)
                saved.setdefault(case, {}).setdefault(str(clients), {})[name] = summary
    assert invocations == expected_invocations
    assert saved == read(f'{run}/candidate-vs-baseline/summary.json')
    assert saved == read('decision.json')['experiments'][run]['summary']
    assert operations == read('decision.json')['experiments'][run]['checked_operations']
    counts[run] = (invocations, operations)
    left, right = [read(f'{run}/{name}-preflight.json') for name in ['baseline', 'candidate']]
    assert left['cases'] == right['cases']
    assert [r for r in left['settings'] if r[0] != 'port'] == [r for r in right['settings'] if r[0] != 'port']
    for case, v in left['cases'].items():
        for i, counters in enumerate(v['counters'], 1):
            assert counters == ([0, i] if case == 'range_scan_100' else [max(0, i - 5), min(5, i)])
assert sum(x[1] for x in counts.values()) == 3990166
confirmation = read('active-control-r2/candidate-vs-baseline/summary.json')['range_scan_100']['8']
b, c = [confirmation[name] for name in ['baseline', 'candidate']]
assert b['tps']['median'] - c['tps']['median'] == 84
assert max(v['tps']['max'] - v['tps']['min'] for v in [b, c]) == 47
assert c['server_cpu_us_per_op']['min'] > b['server_cpu_us_per_op']['max']
assert read('decision.json')['decision'] == 'reject and revert'

# Regenerate the recorded driver outcomes from the XML/list result files.
def collected(directory):
    outcomes = {}
    for p in sorted(directory.rglob('*')):
        if p.suffix == '.lst':
            for line in p.read_text().splitlines():
                state, _, name = line.partition(' ')
                if state in ['PASS', 'FAIL', 'SKIP'] and name:
                    outcomes[name] = state
        elif p.suffix == '.xml':
            for tc in ET.parse(p).getroot().iter('testcase'):
                name = (tc.get('classname') or '') + '::' + (tc.get('name') or '')
                tags = {child.tag for child in tc}
                state = 'FAIL' if tags & {'failure', 'error'} else 'SKIP' if 'skipped' in tags else 'PASS'
                if outcomes.get(name) != 'FAIL':
                    outcomes[name] = state
    return [outcomes[name] + ' ' + name for name in sorted(outcomes)]

migration = root / 'migration-baseline/prisma'
for suite, directory, count, failures in [('nodepg', migration, 149, 3), ('prisma', migration / 'prisma', 47, 0)]:
    values = []
    for name in ['engine', 'postgres']:
        raw = directory / ('nodepg-cache/results' if suite == 'nodepg' else 'cache/results') / name
        result = collected(raw)
        assert result == (directory / (name + '.txt')).read_text().splitlines()
        assert len(result) == count and sum(line.startswith('FAIL ') for line in result) == failures
        assert all(line.startswith(('PASS ', 'FAIL ')) for line in result)
        values.append(result)
    assert values[0] == values[1]
assert read('migration-baseline/runtime.json')['engine']['sha256'] == baseline
assert read('migration-baseline/prisma/nodepg-complete.json')['matching_test_ids'] == 149
assert read('migration-baseline/prisma/prisma/complete.json')['matching_test_ids'] == 47
for relative, metadata, key in [
    ('clients/suites/nodepg.sh', 'migration-baseline/prisma/nodepg-complete.json', 'suite_sha256'),
    ('clients/suites/prisma.sh', 'migration-baseline/prisma/prisma/complete.json', 'suite_sha256'),
    ('clients/suites/prisma.setup.sql', 'migration-baseline/prisma/prisma/complete.json', 'setup_sha256'),
    ('migration-baseline/prisma/nodepg-cache/src/nodepg/yarn.lock',
     'migration-baseline/prisma/nodepg-complete.json', 'yarn_lock_sha256'),
    ('migration-baseline/prisma/prisma/cache/prisma-node/package-lock.json',
     'migration-baseline/prisma/prisma/complete.json', 'package_lock_sha256'),
]:
    assert hashlib.sha256((root / relative).read_bytes()).hexdigest() == read(metadata)[key]

for run in ['candidate-vs-baseline-idle', 'migration-baseline']:
    assert read(f'{run}/complete.json')['exit'] == 0
    for item in read(f'{run}/cleanup.json'):
        if 'pid' in item:
            assert item['pid_absent'] and item['stop_exit'] == item['wrapper_exit'] == 0
        else:
            assert item['stop_exit'] == item['remove_exit'] == 0
            assert item['state']['ExitCode'] == 0 and not item['state']['OOMKilled']
for run in ['active-control', 'active-control-r2']:
    assert read(f'{run}/complete.json')['source_fixture_unchanged']
    for item in read(f'{run}/cleanup.json'):
        if 'pid' in item:
            assert item['stopped'] and item['exit'] == 0
        else:
            assert item['remove_exit'] == 0
live = read('final-live-cleanup.json')
assert len(live['engines_and_e2e']) == 8 and all(p['absent'] for p in live['engines_and_e2e'])
assert len(live['containers']) == 4 and all(c['absent'] for c in live['containers'])
assert live['e2e_remaining_containers'] == [] and live['data_retained']
assert len(live['ports']) == 5 and all(p['connection_refused'] for p in live['ports'])
assert dict(csv.reader((root / 'candidate-gates/status.tsv').open(), delimiter='\t')) == {'build': '0', 'socket': '0', 'hook': '0', 'waits': '0'}
restored = read('restored-build.json')
assert restored['binary_identical_to_verified_baseline'] and restored['binary_sha256'] == baseline
verification = read('verification.json')
ledger = {name: int(code) for name, code in csv.reader((root / 'restored-gates/status.tsv').open(), delimiter='\t')}
assert verification['gates'] == ledger
assert verification['gates']['build'] == verification['gates']['identity'] == 0
assert verification['gates']['test'] == verification['gates']['clippy'] == 0
assert verification['all_source_gates_passed'] == all(code == 0 for code in ledger.values())
def test_summaries(name):
    return [{'state': state, 'passed': int(passed), 'failed': int(failed), 'ignored': int(ignored)}
            for state, passed, failed, ignored in re.findall(
                r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;',
                (root / name).read_text())]
crate_summaries = test_summaries('restored-gates/test.log')
assert crate_summaries == verification['crate_summaries']
assert {key: sum(r[key] for r in crate_summaries) for key in ['passed', 'failed', 'ignored']} == verification['crate_totals'] == {'passed': 430, 'failed': 0, 'ignored': 18}
pg_summaries = test_summaries('restored-gates/pg.log')
assert pg_summaries == verification['pg_summaries']
assert pg_summaries[-1]['passed'] + pg_summaries[-1]['failed'] == 16
assert (ledger['pg'] == 0) == all(r['failed'] == 0 for r in pg_summaries)
pg_log = (root / 'restored-gates/pg.log').read_text()
failure_tail = pg_log.rsplit('\nfailures:\n', 1)[1].split('\n\ntest result:')[0]
failure_ids = [line.strip() for line in failure_tail.splitlines()
               if re.fullmatch(r'\s+[a-z][a-z0-9_]+', line)]
assert failure_ids == verification['pg_failure_ids'] == [
    'point_in_time_branches', 'restore_in_place_keeps_the_endpoint',
    'retention_removes_old_history', 'survives_losing_the_local_disk']
for name in failure_ids:
    output = pg_log.split('---- ' + name + ' stdout ----', 1)[1].split('\n---- ', 1)[0]
    assert 'Operation timed out' in output and 'could not locate required checkpoint record' in output
provenance = read('regression-provenance.json')
assert not provenance['fresh_regression_started'] and provenance['queued_supervisor_cancelled']
assert provenance['reused_source'] == '5b32046d' and provenance['baseline_sha256'] == baseline
lines = (root / 'baseline-regression/results.tsv').read_text().splitlines()
assert '# commit=5b32046d' in lines
regression = list(csv.DictReader([line for line in lines if not line.startswith('#')], delimiter='\t'))
assert len(regression) == 230
assert sum(int(row['units']) for row in regression) == 51150
assert sum(int(row['match']) for row in regression) == 51148
assert all(row['engine_status'] == 'ok' and row['panics'] == '0' for row in regression)
assert {row['file'] for row in regression if row['units'] != row['match']} == {'join', 'partition_prune'}
print('Verified 18 idle windows, 2502 checked fixture queries, 36 active invocations,')
print('3990166 checked operations, 149 matching node-postgres IDs, 47 passing Prisma checks,')
print('candidate rejection, restored binary identity, and recorded cleanup.')
print('PostgreSQL gate exit:', verification['gates']['pg'], '(see README for scope and any failures)')
print('Full regression is reused baseline evidence. Overall product acceptance remains open.')
