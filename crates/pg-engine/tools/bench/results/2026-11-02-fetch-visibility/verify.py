"""Portable audit of saved measurements and qualified acceptance; starts no workloads."""
from pathlib import Path
import datetime
import gzip
import hashlib
import importlib.util
import json
import sys
import tarfile

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent

def check(value, message):
    if not value:
        raise RuntimeError(message)

def read(name):
    return json.loads((ROOT/name).read_text())

def digest(data):
    return hashlib.sha256(data).hexdigest()

def sha(path):
    return hashlib.file_digest(path.open('rb'), 'sha256').hexdigest()

def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT/(name+'.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

def normalized(value):
    return json.loads(json.dumps(value))

def timestamp(value):
    return datetime.datetime.fromisoformat(value).timestamp()

def verify():
    entries = {}
    for line in (ROOT/'SHA256SUMS').read_text().splitlines():
        h, name = line.split('  ', 1)
        path = Path(name)
        check(not path.is_absolute() and '..' not in path.parts and name not in entries,
              'unsafe or duplicate manifest entry')
        check(sha(ROOT/path) == h, 'checksum: '+name)
        entries[name] = h
    check(set(entries) == {str(p.relative_to(ROOT)) for p in ROOT.rglob('*')
                           if p.is_file() and p.name != 'SHA256SUMS'}, 'manifest inventory')
    frame = read('frame.json')
    launch = read('launch-health.json')
    check(sha(ROOT/'frame.json') == launch['frame_sha256'], 'frame launch pin')
    check(launch['timed_samples_before_launch'] == 0, 'samples before freeze')
    frozen = read('frozen-input-map.json')
    archived, omitted = frozen['archived'], frozen['omitted']
    check(set(archived).isdisjoint(omitted), 'overlapping archive map')
    check({p: v['sha256'] for p, v in (archived | omitted).items()}
          == frame['files_sha256'], 'incomplete frozen map')
    check(len(archived) == 693 and len(omitted) == 5, 'frozen coverage')
    members = {v['archive_member']: v['sha256'] for v in archived.values()}
    seen = set()
    with tarfile.open(ROOT/'frozen-inputs.tar.gz', 'r:gz') as archive:
        for item in archive:
            check(item.isfile() and item.name in members and item.name not in seen,
                  'unknown or duplicate archive member')
            check(digest(archive.extractfile(item).read()) == members[item.name], 'frozen archive bytes')
            seen.add(item.name)
    check(seen == members.keys(), 'missing archived input')
    original_root = next(p.removesuffix('/HYPOTHESIS.md') for p in archived
                         if p.endswith('/fetched-fast-path/HYPOTHESIS.md'))
    for original, value in archived.items():
        if original.startswith(original_root+'/'):
            path = ROOT/original.removeprefix(original_root+'/')
            if path.is_file():
                check(sha(path) == value['sha256'], 'export differs from frozen input: '+str(path))
    post_frame = read('post-frame-artifacts.json')
    for name, h in post_frame['scripts_sha256'].items():
        check(original_root+'/'+name not in frame['files_sha256'] and sha(ROOT/name) == h,
              'post-frame audit scripts must not be called prospective gates')
    check(post_frame['superseded_patch'] == 'source-r1.patch'
          and post_frame['trial_patch'] == 'candidate-source/tracked.patch', 'patch provenance')

    build = read('candidate-bin/manifest.json')
    source = build['source_sha256']
    check(len(source) == 2, 'changed-file build coverage')
    for name, h in source.items():
        check(sha(ROOT/'candidate-source'/name) == h, 'candidate source snapshot')
        check(any(p.endswith('/'+name) and v == h for p, v in frame['files_sha256'].items()),
              'source missing from frame')
    patch = ROOT/'candidate-source/tracked.patch'
    check(build['source'] == frame['head']+'+'+sha(patch), 'HEAD+diff identity')
    check(build['binary_sha256'] == frame['candidate_sha256'], 'candidate binary identity')
    deps = read('matched-dependencies.json')
    check(deps['all_fields_equal'] and deps['baseline'] == deps['candidate'], 'dependencies differ')
    check(deps['candidate'] == read('candidate-bin/lib-pg_engine.json')
          == read('baseline-bin/lib-pg_engine.json') and len(deps['candidate']['deps']) == 46,
          'dependency fingerprint copies')
    check(build['build_command'] == ['cargo', 'build', '-p', 'pg-engine', '--release', '--bin',
        'roll-engine', '--example', 'bench', '--example', 'scan_bench']
        and build['environment'] == {'CARGO_PROFILE_RELEASE_DEBUG': 'line-tables-only'},
        'release build configuration')
    check(build['release_build_log_sha256'] == sha(ROOT/'release-build-r1.log'), 'release build log')
    for row in read('pretrial-checks.json'):
        check(row['exit'] == row['execution_evidence']['exit_code'] == 0, 'pretrial exit')
        check(sha(ROOT/row['log']) == row['log_sha256'], 'pretrial log identity')
        check(row['source_sha256']['crates/pg-engine/src/exec/sizing.rs']
              == source['crates/pg-engine/src/exec/sizing.rs'], 'pretrial production source')
        if row['name'] != 'full-crate':
            check(row['source_sha256'] == source, 'current pretrial source')
    check('index_fetches_follow_visibility_changes ... ok' in (ROOT/'current-wire-r1.log').read_text()
          and '68 passed; 0 failed;' in (ROOT/'current-wire-r1.log').read_text(), 'current wire tests')
    clippy = load('check_clippy')
    clippy.BASE = ROOT/'baseline-clippy.log'
    check(normalized(clippy.verify(ROOT/'clippy-r1.log')) == read('clippy-comparison.json'),
          'pretrial warnings')
    check(normalized(clippy.verify(ROOT/'source-gates/clippy-all-targets.log'))
          == read('source-gates/clippy-warning-comparison.json'), 'final warnings')

    evaluator, auditor = load('evaluate'), load('audit_pairs')
    evaluator.self_test()
    total = invocations = 0
    previous_end = timestamp(launch['at_utc'])
    phase_specs = [('fetch-primary-p1', 'fetch-primary', None, False),
                   ('control-dirty', 'control-dirty', 'dirty', False),
                   ('fetch-confirmation-p1', 'fetch-confirmation', None, True)]
    for phase, saved, control, swapped in phase_specs:
        run = ROOT/phase
        stats = evaluator.evaluate(run, control, swapped)
        check(stats == read(saved+'-statistics.json') and stats['performance_rule_passes'],
              'statistics or prospective rule: '+phase)
        actual = auditor.audit(run)
        saved_audit = read(saved+'-accounting.json')
        actual.pop('run')
        saved_audit.pop('run')
        check(actual == saved_audit, 'accounting: '+phase)
        total += actual['checked_operations_including_warmups']
        invocations += actual['invocations_including_warmups']
        check(previous_end < read(phase+'/started.json')['at_unix'], 'phase order')
        previous_end = timestamp(read(phase+'/complete.json')['at_utc'])
        rt = read(phase+'/runtime.json')
        check(rt['source_sha256'] == source, 'runtime source identity')
        check(rt['engines']['candidate']['sha256'] == frame['candidate_sha256']
              and rt['engines']['baseline']['sha256'] == frame['baseline_sha256'], 'live binary hashes')
        for engine in rt['engines'].values():
            check(engine['nice'] == 19 and engine['affinity'] == [28, 29, 30, 31]
                  and all(a == [28, 29, 30, 31] for a in engine['thread_affinities']), 'runtime limits')
        config = read(phase+'/candidate-vs-baseline/config.json')
        check([v['name'] for v in config['targets']] == (
              ['candidate', 'baseline'] if swapped else ['baseline', 'candidate']), 'target order')
        check(config['runner_sha256'] == '03542748e7c220c3d15e3221b4951bc7fec0834710ae50ee9b0154ea464ac46c'
              and rt['client_sha256'] == config['bench_sha256']
              == '9f5b02222402fd5b09772088ce704054299339bb80723f1ef15fd915fb0b8e28', 'shared client')
        for side in ['baseline', 'candidate']:
            for moment in ['before', 'after']:
                check((run/'candidate-vs-baseline'/f'fixture-{moment}-{side}.txt').read_text().strip()
                      == config['expected_fixture'] == rt['expected_fixture'], 'logical fixture')
        observation = run/'host-cpu-observation.jsonl.gz'
        with gzip.open(observation, 'rt') as stream:
            first = json.loads(next(stream))
        check(first['at_unix'] <= timestamp(rt['at_utc']), 'observer must precede runtime capture')
    check((invocations, total) == (128, 19178971), 'total checked workload')
    check(read('performance-decision.json')['performance_checks_pass']
          and read('performance-decision.json')['action'] == 'source_checks_pending'
          and not read('performance-decision.json')['source_acceptance'], 'original performance decision')
    for phase, control, swapped in [('monitor-diagnostic-r1', None, False),
            ('dirty-diagnostic-r1', 'dirty', False), ('swap-diagnostic-r1', None, True),
            ('monitor-correctness', None, False)]:
        rt, complete = read(phase+'/runtime.json'), read(phase+'/complete.json')
        check(rt['control_phase'] == control and rt['slots_swapped'] == swapped, 'diagnostic mode')
        check(not complete['benchmark_executed'] and complete['source_fixture_unchanged'], 'untimed diagnostics')
        check(read(phase+'/baseline-monitor-correctness.json')
              == read(phase+'/candidate-monitor-correctness.json'), 'monitor parity')

    regression = load('check_regression').verify(ROOT/'full-regress-fetch-p1/report',
                                                ROOT/'baseline-regression/results.tsv')
    check(regression == read('full-regress-fetch-p1/acceptance.json'), 'regression acceptance')
    check(read('full-regress-fetch-p1/identity.json')['engine_sha256'] == frame['candidate_sha256'],
          'regression candidate')
    check(read('full-regress-fetch-p1/identity.json')['baseline_results_sha256']
          == sha(ROOT/'baseline-regression/results.tsv'), 'regression baseline identity')
    check(all(r['exit'] == 0 for r in read('full-regress-fetch-p1/cleanup.json')), 'regression cleanup')
    q = load('qualify_source_gates').qualify(ROOT, ROOT/'baseline-service/postgres.txt',
                                           ROOT/'baseline-service/ACCEPTANCE.md')
    check(q == read('source-gate-qualification.json'), 'failed service gate qualification')
    live = read('final-live-cleanup.json')
    check(len(live['owned_pids_absent']) == 8 and len(live['owned_helper_ids_absent']) == 4
          and len(live['removed_owned_paths']) == 8 and live['frame_still_matches']
          and live['source_fixture_unchanged_across_all_phases'], 'measurement cleanup record')
    gates_live = read('source-gates-live-cleanup.json')
    check(gates_live['frame_inputs_unchanged'] == 698 and gates_live['service_state_removed']
          and not gates_live['worktree_service_containers'], 'source cleanup record')
    decision = read('final-decision.json')
    check(decision['action'] == 'retain_under_existing_recovery_waiver', 'bounded disposition')
    check(not decision['full_source_acceptance'] and not decision['product_acceptance'],
          'failed service gate cannot become full acceptance')
    check(decision['performance_checks_pass'] and decision['existing_recovery_waiver_explicit'],
          'qualified decision scope')
    rows = (ROOT/'decisions.tsv').read_text().splitlines()
    check(rows[0].split('\t') == ['ts', 'phase', 'decision', 'why', 'evidence', 'result']
          and all(len(r.split('\t')) == 6 for r in rows[1:]), 'trail shape')
    return {'files': len(entries), 'frozen_inputs_rehashed_offline': len(archived),
            'frozen_binaries_omitted': len(omitted), 'performance_checks_pass': True,
            'invocations': invocations, 'checked_operations': total, 'errors': 0,
            'regression_new_differences': 0, 'full_source_acceptance': False,
            'service_failures': q['failed_tests'], 'product_acceptance': False,
            'post_frame_audit_scripts': sorted(post_frame['scripts_sha256']),
            'limit': 'Audits saved records only. Does not rerun binaries/tests, verify current live state, '
                     'rehash omitted binaries/databases, or establish PostgreSQL replacement.'}

if __name__ == '__main__':
    print(json.dumps(verify(), indent=2))
