"""Check only owned regression/service resources and remove the private engine copy/data."""
from pathlib import Path
import hashlib
import json
import shutil
import socket
import subprocess
import time

ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')

def check(value, message):
    if not value:
        raise RuntimeError(message)

def sha(path):
    return hashlib.file_digest(path.open('rb'), 'sha256').hexdigest()

def main():
    run = ROOT/'full-regress-fetch-p1'
    check(json.loads((run/'status.json').read_text())['exit'] == 0, 'regression unfinished')
    check(json.loads((run/'acceptance.json').read_text())['passes'], 'regression not accepted')
    pgr = run/'pgr'
    pids = [int((pgr/'postmaster.pid').read_text().splitlines()[0]),
            json.loads((run/'harness-owned.json').read_text())['pid']]
    check(all(not Path('/proc', str(p)).exists() for p in pids), 'owned process remains')
    containers = [r['id'] for r in json.loads((run/'cleanup.json').read_text())]
    for cid in containers:
        r = subprocess.run(['docker', 'inspect', cid], capture_output=True, text=True)
        check(r.returncode != 0 and 'no such' in r.stderr.lower(), 'owned container remains')
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 53720))
    status = json.loads((ROOT/'source-gates/status.json').read_text())
    pg = next(r for r in status if r['name'] == 'postgres')
    logs = [p for p in (REPO/'target/tmp').glob('rolld-service-*.log')
            if pg['started_unix'] <= p.stat().st_mtime <= pg['ended_unix']]
    check(len(logs) == 1, 'cannot identify this service run log')
    service_pid = int(logs[0].stem.rsplit('-', 1)[1])
    check(not Path('/proc', str(service_pid)).exists(), 'service test process remains')
    check(not logs[0].with_suffix('').exists(), 'successful service state was not removed')
    shutil.copy2(logs[0], ROOT/'source-gates/daemon.log')
    r = subprocess.run(['docker', 'ps', '-aq', '--filter', 'label=roll.home'],
                       capture_output=True, text=True, check=True)
    homes = []
    for cid in r.stdout.split():
        info = json.loads(subprocess.check_output(['docker', 'inspect', cid]))[0]
        home = info['Config']['Labels']['roll.home']
        if Path(home).is_relative_to(REPO/'target/tmp'):
            homes.append({'id': cid, 'home': home})
    check(not homes, 'service computes remain in this worktree')
    frame = json.loads((ROOT/'frame.json').read_text())
    check(all(sha(Path(p)) == h for p, h in frame['files_sha256'].items()), 'frame changed')
    binary = pgr/'roll-engine.bin'
    check(sha(binary) == json.loads((ROOT/'candidate-bin/manifest.json').read_text())['binary_sha256'],
          'private regression binary identity')
    removed = []
    for path in [pgr/'enginedata', binary]:
        check(path.parent == pgr and not path.is_symlink(), 'unsafe private path')
        if path.is_dir():
            shutil.rmtree(path)
        else:
            path.unlink()
        removed.append(str(path))
    value = {'at_unix': time.time(), 'regression_pids_absent': pids,
             'regression_container_ids_absent': containers, 'regression_port_binds': 53720,
             'service_test_pid_absent': service_pid, 'service_state_removed': True,
             'worktree_service_containers': homes, 'frame_inputs_unchanged': len(frame['files_sha256']),
             'removed_private_paths': removed,
             'limit': 'Observed after gates; no claim about unrelated containers, old service state, or future live state.'}
    (ROOT/'source-gates-live-cleanup.json').write_text(json.dumps(value, indent=2)+'\n')
    print(json.dumps(value, indent=2))

if __name__ == '__main__':
    main()
