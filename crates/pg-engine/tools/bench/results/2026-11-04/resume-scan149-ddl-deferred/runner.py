#!/usr/bin/env python3
"""One unchanged binary check, only after the pinned owner jobs terminate."""
import datetime, hashlib, json, os, re, resource, subprocess, sys, threading, time
from pathlib import Path
base = Path('/home/meszmate/.cache/pg-engine-resume-20261104/execution_models')
wt = '/home/meszmate/roll/.claude/worktrees/agent-a814fa71b51112148'
out = base / 'a149-ddl-isolated'
out.mkdir(exist_ok=False)

def save(name, value):
    (out / name).write_text(json.dumps(value, indent=2) + '\n')

def git(*args):
    return subprocess.check_output(['git', '-C', wt, *args], text=True).strip()

def source():
    return dict(head=git('rev-parse', 'HEAD'), tree=git('rev-parse', 'HEAD^{tree}'),
                status=git('status', '--porcelain=v1'),
                patch_sha256=hashlib.sha256(git('diff', '--binary', 'HEAD').encode()).hexdigest())

def pressure():
    return dict(at_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                load=Path('/proc/loadavg').read_text(),
                psi={k: Path('/proc/pressure/' + k).read_text() for k in ['cpu', 'io', 'memory']})

def tasks():
    return subprocess.check_output(['ps', '-eo', 'pid,ppid,ni,psr,pcpu,etime,args', '--sort=-pcpu'], text=True)

pin = source()
save('source.json', pin)
assert pin['head'] == '26f4bfac98647989ecff63ef234ec2a8a96d5771' and not pin['status'], pin
cpus = set(os.sched_getaffinity(0))
assert cpus <= {4,5,6,7,22,23,24,25} and os.getpriority(os.PRIO_PROCESS, 0) == 19
save('execution.json', dict(effective_cpus=sorted(cpus), nice=19,
    CARGO_BUILD_JOBS=os.environ.get('CARGO_BUILD_JOBS'), RUST_TEST_THREADS=os.environ.get('RUST_TEST_THREADS')))
for name in ['a167-review-final', 'a170-review-final']:
    log = (base / (name + '-launch.log')).read_text()
    assert re.search(r'^HEAVY-EXIT \d+$', log, re.M), name + ' still active'

# Refuse overlapping builds, tests or engine processes on the reserved remaining cores.
competing = []
for item in Path('/proc').iterdir():
    if not item.name.isdigit():
        continue
    try:
        pid = int(item.name)
        cmd = (item / 'cmdline').read_bytes().replace(b'\0', b' ').decode(errors='replace')
        if re.search(r'(?:/rustc |/cargo (?:test|build|clippy)|/target/(?:debug|release|dist)/)', cmd):
            affinity = set(os.sched_getaffinity(pid))
            if affinity & cpus:
                competing.append(dict(pid=pid, cmd=cmd, effective_cpus=sorted(affinity)))
    except (OSError, ProcessLookupError):
        continue
save('preflight-pressure.json', pressure())
(out / 'preflight-tasks.txt').write_text(tasks())
save('overlapping-heavy-tasks.json', competing)
if competing:
    save('result.json', dict(status='DEFERRED', reason='overlapping heavy process affinity; target never launched'))
    sys.exit(3)

binary = Path('/home/meszmate/.cache/a149/target/debug/deps/ddl_scaling-6e4f0e25cd2adb62')
digest = hashlib.sha256(binary.read_bytes()).hexdigest()
assert digest == '90e9991465d10cb573b5fdbe054d21ba7a27421aed70074936817742133948d9', digest
cmd = [str(binary), 'racing_ddl_commits_dont_grow_with_the_catalog', '--exact', '--nocapture']
save('command.json', dict(argv=cmd, binary_sha256=digest, bytes=binary.stat().st_size))
usage = resource.getrusage(resource.RUSAGE_CHILDREN)
started = time.monotonic()
samples = [pressure()]
with (out / 'raw.log').open('w') as log:
    proc = subprocess.Popen(cmd, cwd=wt, stdout=log, stderr=subprocess.STDOUT)
    child_affinity = sorted(os.sched_getaffinity(proc.pid))
    done = threading.Event()
    def sample():
        while not done.wait(0.5):
            samples.append(pressure())
    monitor = threading.Thread(target=sample)
    monitor.start()
    code = proc.wait()
    wall = time.monotonic() - started
    done.set()
    monitor.join()
end_usage = resource.getrusage(resource.RUSAGE_CHILDREN)
raw = (out / 'raw.log').read_text(errors='replace')
result = dict(exit=code, wall_s=wall, user_cpu_s=end_usage.ru_utime-usage.ru_utime,
    system_cpu_s=end_usage.ru_stime-usage.ru_stime, child_effective_cpus=child_affinity,
    test_summaries=re.findall(r'^test result:.*$', raw, re.M),
    pass_gate=code == 0 and bool(re.search(r'^test result: ok\. 1 passed;', raw, re.M)),
    source_after=source(), binary_sha256_after=hashlib.sha256(binary.read_bytes()).hexdigest(),
    limitation='ONE isolated target only. Original full debug exit101 remains failed; no historical attribution or overall green.')
save('samples.json', samples)
(out / 'final-tasks.txt').write_text(tasks())
save('result.json', result)
assert result['source_after'] == pin and result['binary_sha256_after'] == digest
print(json.dumps(result), flush=True)
sys.exit(code or (0 if result['pass_gate'] else 1))
