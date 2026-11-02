"""Private full candidate regression; preserve the historical baseline separately."""

def check(condition, message):
    if not condition:
        raise RuntimeError(message)

from pathlib import Path
import hashlib
import json
import os
import shutil
import socket
import subprocess
import time
import signal

ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')
BASE = Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/final-regress-5b32046d-release')
IMAGE = 'postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873'
OUT = ROOT / 'full-regress-proof-p1'
PORT = 53562
HARNESS = REPO / 'target/release/deps/pg_regress-fd39d3793202ae64'
ENGINE = ROOT / 'candidate-bin/roll-engine'

def sha(path):
    return hashlib.file_digest(path.open('rb'), 'sha256').hexdigest()

def save(name, value):
    (OUT / name).write_text(json.dumps(value, indent=2) + '\n')

def command(args, **kwargs):
    return subprocess.run(args, text=True, capture_output=True, timeout=60, check=True, **kwargs).stdout.strip()

check(json.loads((ROOT / 'performance-decision.json').read_text())['action'] == 'source_checks_pending', 'Regression input validation failed at original line 30')
check(sha(HARNESS) == '311bd683d5d1aa96f73b946e975bd31912fbdb4327afbc7218ad4167e7647fb6', 'Regression input validation failed at original line 31')
check(sha(ENGINE) == json.loads((ROOT / 'candidate-bin/manifest.json').read_text())['binary_sha256'], 'Regression input validation failed at original line 32')
with socket.socket() as sock:
    sock.bind(('127.0.0.1', PORT))
OUT.mkdir(exist_ok=False)
pgr = OUT / 'pgr'
pgr.mkdir()
shutil.copytree(BASE / 'pgr/src-repo', pgr / 'src-repo')
owned, cleanup = [], []
harness = None

def interrupted(signum, frame):
    raise KeyboardInterrupt('signal ' + str(signum))

signal.signal(signal.SIGTERM, interrupted)
try:
    shared = ['--label', 'agent.worktree=t3-d4020b98', '--label', 'agent.task=gather-proof-regress-p1']
    ref = command(['docker', 'run', '-d', '--name', 't3-d4020b98-proof-p1-ref'] + shared +
                  ['--memory', '2g', '-e', 'POSTGRES_PASSWORD=pw', '-e',
                   'POSTGRES_INITDB_ARGS=--locale=C --encoding=UTF8', IMAGE])
    owned.append(ref)
    helper = command(['docker', 'run', '-d', '--name', 't3-d4020b98-proof-p1-psql'] + shared +
                     ['--memory', '256m', '--network', 'host', '--user', f'{os.getuid()}:{os.getgid()}',
                      '-v', f'{pgr}:{pgr}', '--entrypoint', 'sleep', IMAGE, 'infinity'])
    owned.append(helper)
    save('owned.json', {'reference_id': ref, 'helper_id': helper, 'image': IMAGE, 'port': PORT})
    for attempt in range(120):
        ready = subprocess.run(['docker', 'exec', ref, 'pg_isready', '-U', 'postgres'],
                               capture_output=True, timeout=10)
        if ready.returncode == 0:
            break
        time.sleep(.5)
    else:
        raise RuntimeError('reference SQL readiness failed')
    env = dict(os.environ, PGREGRESS_DIR=str(pgr), PGREGRESS_DB='regression_scan_p1',
               PGREGRESS_PORT=str(PORT), PGREGRESS_CONTAINER=ref,
               PGREGRESS_ENGINE_CONTAINER=helper, PGREGRESS_REPORT_DIR=str(OUT / 'report'),
               PGREGRESS_ENGINE_BIN=str(ENGINE), PGREGRESS_COMMIT=json.loads((ROOT / 'candidate-bin/manifest.json').read_text())['source'],
               PGREGRESS_BASELINE=str(BASE / 'report/results.tsv'), PGREGRESS_UPDATE_STATUS='0')
    args = ['timeout', '--kill-after=30', '10800', str(HARNESS), 'pg_regress', '--exact', '--ignored', '--nocapture']
    save('identity.json', {'engine_sha256': sha(ENGINE), 'harness_sha256': sha(HARNESS),
                           'baseline_kind': 'reused full release run at 5b32046d; production source equal to HEAD61a87d6f',
                           'baseline_results_sha256': sha(BASE / 'report/results.tsv'),
                           'command': args, 'environment': {k:v for k,v in env.items() if k.startswith('PGREGRESS_')}})
    started = time.time()
    with (OUT / 'regress.log').open('x') as log:
        harness = subprocess.Popen(args, env=env, cwd=REPO, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        save('harness-owned.json', {'pid': harness.pid, 'pgid': harness.pid, 'command': args})
        code = harness.wait()
    save('status.json', {'exit': code, 'started_unix': started, 'ended_unix': time.time()})
finally:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    if harness is not None and harness.poll() is None:
        os.killpg(harness.pid, signal.SIGTERM)
        try:
            harness.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.killpg(harness.pid, signal.SIGKILL)
            harness.wait(timeout=10)
    for container in reversed(owned):
        try:
            result = subprocess.run(['docker', 'rm', '-fv', container], text=True, capture_output=True, timeout=60)
            cleanup.append({'id': container, 'exit': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr})
        except Exception as error:
            cleanup.append({'id': container, 'error': repr(error)})
    save('cleanup.json', cleanup)

raise SystemExit(code if all(row.get('exit') == 0 for row in cleanup) else 1)
