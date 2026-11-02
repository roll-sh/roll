"""Execute only the prospective frozen experiment; source acceptance is separate."""

def check(condition, message):
    if not condition:
        raise RuntimeError(message)

from pathlib import Path
import hashlib
import importlib.util
import json
import os
import signal
import subprocess
import sys
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')


def save(name, value):
    with (ROOT / name).open('x') as f:
        json.dump(value, f, indent=2)
        f.write('\n')


def sha(path):
    return hashlib.file_digest(Path(path).open('rb'), 'sha256').hexdigest()


FRAME_DIGEST = sha(ROOT / 'frame.json')

def verify_frame():
    check(sha(ROOT / 'frame.json') == FRAME_DIGEST, 'frame changed during experiment')
    frame = json.loads((ROOT / 'frame.json').read_text())
    for path, digest in frame['files_sha256'].items():
        check(sha(path) == digest, 'frozen input changed: ' + path)
    return frame


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def phase(name, args, timeout):
    verify_frame()
    command = [str(REPO / 'crates/pg-engine/tools/heavy.sh'), 'python3', '-I',
               str(ROOT / 'measure.py'), '--out', str(ROOT / name),
               '--candidate-dir', str(ROOT / 'candidate-bin')] + args
    start = time.time()
    with (ROOT / (name + '-controller.log')).open('x') as log:
        process = subprocess.Popen(command, cwd=REPO, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        save(name + '-controller-owned.json', {'pid': process.pid, 'pgid': process.pid,
                                             'command': command})
        try:
            # The shared heavy slot may queue indefinitely. The phase budget starts
            # only after the actual harness has acquired it and written its marker.
            deadline = None
            while process.poll() is None:
                if deadline is None and (ROOT / name / 'started.json').exists():
                    deadline = time.monotonic() + timeout
                if deadline is not None and time.monotonic() > deadline:
                    raise subprocess.TimeoutExpired(command, timeout)
                time.sleep(.5)
            code = process.returncode
        except BaseException as error:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=10)
            save(name + '-status.json', {'command': command, 'exit': process.returncode,
                                        'started_unix': start, 'ended_unix': time.time(),
                                        'error': repr(error)})
            raise
    save(name + '-status.json', {'command': command, 'exit': code,
                                'started_unix': start, 'ended_unix': time.time()})
    check(code == 0, name + ' harness failed')
    cleanup = json.loads((ROOT / name / 'cleanup.json').read_text())
    check(len(cleanup) == 3, name + ' incomplete cleanup')
    check(all(row.get('stopped') and row.get('exit') == 0 for row in cleanup if 'pid' in row), name + ' engine cleanup failed')
    check(all(row.get('remove_exit') == 0 for row in cleanup if 'helper_id' in row), name + ' helper cleanup failed')
    observation = json.loads((ROOT / name / 'host-observation.json').read_text())
    check(observation['stopped'] and not observation['errors'], name + ' observer failed')
    complete = json.loads((ROOT / name / 'complete.json').read_text())
    check(complete['source_fixture_unchanged'], name + ' source fixture changed')
    check(complete['benchmark_executed'] == (name != 'monitor-correctness'), name + ' wrong phase work')
    verify_frame()


def experiment():
    verify_frame()
    phase('monitor-correctness', ['--preflight-only', '--monitor', '--port0', '53700'], 600)
    phase('fetch-primary-p1', ['--port0', '53702'], 2400)
    statistics = load('evaluate')
    audit = load('audit_pairs')
    primary = statistics.evaluate(ROOT / 'fetch-primary-p1')
    accounting = audit.audit(ROOT / 'fetch-primary-p1')
    save('fetch-primary-statistics.json', primary)
    save('fetch-primary-accounting.json', accounting)
    if not primary['performance_rule_passes']:
        save('performance-decision.json', {'action': 'reject', 'reason': 'primary locked rule failed',
                                         'source_acceptance': False, 'remaining_controls_skipped': True})
        sys.exit(0)
    all_pass = True
    failed_phase = None
    for index, control_name in enumerate(['dirty']):
        name = 'control-' + control_name
        phase(name, ['--control',control_name,'--port0',str(53704+index*2)], 1800)
        result = statistics.evaluate(ROOT / name, control=control_name)
        accounting = audit.audit(ROOT / name)
        save(name + '-statistics.json', result)
        save(name + '-accounting.json', accounting)
        all_pass = all_pass and result['performance_rule_passes']
        if not all_pass:
            failed_phase = name
            break
    if all_pass:
        phase('fetch-confirmation-p1', ['--swap-slots', '--port0', '53706'], 2400)
        confirmation = statistics.evaluate(ROOT / 'fetch-confirmation-p1', swapped=True)
        save('fetch-confirmation-statistics.json', confirmation)
        save('fetch-confirmation-accounting.json', audit.audit(ROOT / 'fetch-confirmation-p1'))
        all_pass = confirmation['performance_rule_passes']
        if not all_pass:
            failed_phase = 'fetch-confirmation-p1'
    save('performance-decision.json', {'action': 'source_checks_pending' if all_pass else 'reject',
        'performance_checks_pass': all_pass, 'source_acceptance': False,
        'reason': 'primary, dirty control and swapped-slot confirmation passed' if all_pass else failed_phase + ' failed'})

try:
    experiment()
except BaseException as error:
    if not (ROOT / 'performance-decision.json').exists():
        save('performance-decision.json', {'action':'reject','source_acceptance':False,
             'performance_checks_pass':False,'reason':'harness or evaluator failure',
             'error':repr(error),'relaunch_allowed':False})
    raise
