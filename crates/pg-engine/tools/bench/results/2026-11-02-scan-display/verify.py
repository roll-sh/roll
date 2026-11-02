"""Audit exported records; never start a server or benchmark."""
import hashlib
import importlib.util
import json
import csv
from pathlib import Path
import sys

sys.dont_write_bytecode = True


def module(root, name):
    spec = importlib.util.spec_from_file_location(name, root / (name + '.py'))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def audit_records(root):
    pairs = module(root, 'audit_pairs')
    statistics = module(root, 'p1_statistics')
    statistics.self_test()
    original = []
    for name in ['paired-r1', 'paired-r3', 'paired-r4']:
        result = pairs.audit(root / name)
        result['run'] = name
        assert not result['declared_performance_rule_passes']
        original.append(result)
    failure = json.loads((root / 'paired-r2/failure.json').read_text())
    assert failure['observations'] == 0
    p1 = pairs.audit(root / 'paired-p1')
    p1['run'] = 'paired-p1'
    prospective = statistics.evaluate(root / 'paired-p1')
    freeze = json.loads((root / 'p1-frame.json').read_text())
    for name, expected in freeze['tool_sha256'].items():
        assert hashlib.sha256((root / Path(name).name).read_bytes()).hexdigest() == expected
    final = json.loads((root / 'candidate-final-bin/manifest.json').read_text())
    assert final['source_sha256'] == freeze['source_sha256']
    for name, expected in final['source_sha256'].items():
        assert hashlib.sha256((root / 'source-p1' / name).read_bytes()).hexdigest() == expected
    runtime = json.loads((root / 'paired-p1/runtime.json').read_text())
    assert runtime['engines']['candidate']['sha256'] == final['binary_sha256']
    assert runtime['source_sha256'] == final['source_sha256']
    assert runtime['engines']['baseline']['sha256'] == '5cfc79043a50d0ac228bdac3c66deb5fcf0dd34d73add389bf2773237e51ffd1'
    for name in ['baseline', 'candidate']:
        assert runtime['engines'][name]['affinity'] == [28, 29, 30, 31]
    deps = json.loads((root / 'matched-dependencies-final.json').read_text())
    assert deps['dependency_fingerprints_equal']
    assert deps['baseline_fingerprint'] == deps['candidate_fingerprint']
    rebuild = json.loads((root / 'rebuild-identity.json').read_text())
    assert rebuild['allocated_sections_except_build_id_equal'] and rebuild['old'] == rebuild['new']
    observer = json.loads((root / 'paired-p1/host-observation.json').read_text())
    assert observer['stopped'] and observer['errors'] == []
    gates = root / 'source-gates/status.json'
    gate_results = json.loads(gates.read_text()) if gates.exists() else []
    decision = json.loads((root / 'decision.json').read_text())
    assert decision['p1_performance_rule_passes'] == prospective['p1_statistical_rule_passes']
    assert decision['product_accepted'] is False
    assert decision['candidate_accepted'] is False and decision['action'] == 'reverted'
    assert not prospective['p1_statistical_rule_passes']
    skipped = json.loads((root / 'source-gates-skipped.json').read_text())
    assert skipped['source_checks_executed'] is False and gate_results == []
    restored = json.loads((root / 'restored-build.json').read_text())
    assert restored['exit'] == 0 and restored['binary_matches_verified_baseline']
    assert restored['binary_sha256'] == runtime['engines']['baseline']['sha256']
    restoration = json.loads((root / 'restoration.json').read_text())
    assert restoration['runtime_source_diff_to_baseline'] == ''
    live = json.loads((root / 'final-live-cleanup.json').read_text())
    assert len(live['pids']) == 2 and all(p['absent'] for p in live['pids'])
    assert len(live['containers']) == 1 and all(c['absent'] for c in live['containers'])
    assert {p['port'] for p in live['ports']} == {53470, 53471}
    assert all(p['bind_available'] for p in live['ports'])
    clippy = json.loads((root / 'restored-clippy.json').read_text())
    assert clippy['exit'] == 0 and clippy['command'] == ['cargo', 'clippy']
    assert 'Finished `dev` profile' in (root / clippy['log']).read_text()
    trail = list(csv.DictReader((root / 'decisions.tsv').read_text().splitlines(), delimiter='\t'))
    phases = [row['phase'] for row in trail]
    for phase in ['scan-display-p1-lock', 'scan-display-p1-reject',
                  'scan-display-restored-verified', 'scan-display-cleanup-audit-correction',
                  'scan-display-evidence-fixes']:
        assert phase in phases
    assert phases.index('scan-display-p1-lock') < phases.index('scan-display-p1-reject')
    assert phases.index('scan-display-restored-verified') < phases.index('scan-display-cleanup-audit-correction')
    return {'original_trials': original, 'p1_accounting': p1,
            'p1_statistics': prospective, 'source_gate_results': gate_results,
            'decision': decision, 'restored_build': restored, 'restored_clippy': clippy,
            'captured_final_cleanup': live,
            'limits': 'Recomputes saved records. No services, workloads, original ELF files or fixture databases are rerun or rehashed.'}


def check_manifest(root):
    entries = {}
    for line in (root / 'SHA256SUMS').read_text().splitlines():
        expected, name = line.split('  ', 1)
        relative = Path(name)
        assert not relative.is_absolute() and '..' not in relative.parts
        assert name not in entries
        assert hashlib.sha256((root / relative).read_bytes()).hexdigest() == expected, name
        entries[name] = expected
    actual = {str(p.relative_to(root)) for p in root.rglob('*')
              if p.is_file() and p.name != 'SHA256SUMS'}
    assert set(entries) == actual
    return len(entries)


if __name__ == '__main__':
    root = Path(__file__).resolve().parent
    count = check_manifest(root)
    result = audit_records(root)
    assert result == json.loads((root / 'verification.json').read_text())
    print(json.dumps({'manifest_files': count, 'p1': result['p1_statistics'],
                      'source_gates': [(g['name'], g['exit']) for g in result['source_gate_results']],
                      'decision': result['decision']}, indent=2))
