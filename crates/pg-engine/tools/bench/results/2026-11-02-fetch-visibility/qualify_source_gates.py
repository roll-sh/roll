"""Record the existing service gap without turning a failed gate into a pass."""
from pathlib import Path
import hashlib
import json
import re

ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')
BASE = REPO / 'crates/rolld/tests/results/2026-11-02-service/postgres.txt'
EXPECTED = sorted(['point_in_time_branches', 'restore_in_place_keeps_the_endpoint',
                   'retention_removes_old_history', 'survives_losing_the_local_disk'])

def check(value, message):
    if not value:
        raise RuntimeError(message)

def sha(path):
    return hashlib.file_digest(path.open('rb'), 'sha256').hexdigest()

def failures(path):
    text = path.read_text()
    names = sorted(re.findall(r'^test (\w+) \.\.\. FAILED$', text, re.M))
    check(names == EXPECTED, 'unexpected service failure set')
    check('12 passed; 4 failed;' in text, 'missing completed service summary')
    for name in EXPECTED:
        match = re.search(r'^---- ' + name + r' stdout ----\n(.*?)(?=^---- |^failures:)',
                          text, re.M | re.S)
        check(match is not None, 'missing failure details: ' + name)
        check('compute WAL preflight expected HTTP 403 for a wrong token' in match[1]
              and 'wget: download timed out' in match[1], 'different cause: ' + name)
    check('test serves_requests_and_adopts_compute_after_restart ... ok' in text,
          'daemon lifecycle missing')
    return names

def qualify(root=ROOT, base=BASE, acceptance=REPO/'crates/pg-engine/ACCEPTANCE.md'):
    fresh = root / 'source-gates/postgres.log'
    check(failures(fresh) == failures(base), 'baseline failure comparison differs')
    check('Preserve the current recovery-test waiver as an open gap, not a passing gate.'
          in acceptance.read_text(), 'existing waiver is not documented')
    status = json.loads((root/'source-gates/status.json').read_text())
    complete = json.loads((root/'source-gates/complete.json').read_text())
    check({r['name']: r['exit'] for r in status} == {
        'build': 0, 'test-rest-of-workspace': 0, 'clippy': 0,
        'clippy-all-targets': 0, 'postgres': 101, 'regression': 0}, 'unexpected gate exits')
    check(complete['all_checks_ran'] and not complete['all_exit_zero'],
          'failed gate must stay failed')
    result = {'all_source_gates_pass': False, 'performance_checks_pass': True,
        'fresh_service_exit': 101, 'control_plane_passed': 12, 'control_plane_failed': 4,
        'failed_tests': EXPECTED, 'failure_signature_matches_retained_baseline': True,
        'fresh_log_sha256': sha(fresh), 'baseline_log_sha256': sha(base),
        'acceptance_document_sha256': sha(acceptance),
        'scope': 'Existing official-PostgreSQL compute-to-host WAL-route gap; recovery unverified. '
                 'This records a waiver already documented before this experiment, not a passing gate '
                 'or an amendment to the frozen performance rules.',
        'decision': 'Independent review and explicit bounded-engine decision still required.'}
    return result

if __name__ == '__main__':
    value = qualify()
    (ROOT/'source-gate-qualification.json').write_text(json.dumps(value, indent=2)+'\n')
    print(json.dumps(value, indent=2))
