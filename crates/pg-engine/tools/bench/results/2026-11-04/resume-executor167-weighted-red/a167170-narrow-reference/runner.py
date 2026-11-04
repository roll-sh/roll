#!/usr/bin/env python3
"""Owned, bounded PG18.6 reference only; no performance measurement."""
import hashlib, json, os, re, subprocess, time
from pathlib import Path

base = Path('/home/meszmate/.cache/pg-engine-resume-20261104/execution_models')
wt = Path('/home/meszmate/roll/.claude/worktrees/agent-a6193f5e45a7ae4f1')
out = base / 'a167170-narrow-reference'
out.mkdir(exist_ok=False)
name = 'a167170-resume-reference'
image = 'sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873'
checks = []

def run(stage, argv, data=None, env=None, want=0):
    p = subprocess.run(argv, cwd=wt, input=data, text=True,
                       env=dict(os.environ, **(env or {})), stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    (out / (stage + '.log')).write_text(p.stdout)
    checks.append(dict(stage=stage, argv=argv, env=env, exit=p.returncode, expected_exit=want,
                       sha256=hashlib.sha256(p.stdout.encode()).hexdigest()))
    (out / 'checks.json').write_text(json.dumps(checks, indent=2) + '\n')
    print(json.dumps(checks[-1]), flush=True)
    assert p.returncode == want, (stage, p.returncode, p.stdout[-1500:])
    return p.stdout

assert os.getpriority(os.PRIO_PROCESS, 0) == 19
assert set(os.sched_getaffinity(0)) <= {4,5,6,7,22,23,24,25}
(out / 'execution.json').write_text(json.dumps(dict(nice=19, effective_cpus=sorted(os.sched_getaffinity(0))), indent=2))
for owner in ['a167','a170']:
    assert 'BATCH_CHECKS_PASSED\nHEAVY-EXIT 0' in (base / (owner + '-review-final-launch.log')).read_text()
assert not run('existing-owned', ['docker','ps','-aq','--filter','name=^/' + name + '$']).strip()
run('image', ['docker','image','inspect',image])
created = False
try:
    run('create', ['docker','run','--rm','-d','--name',name,
        '--label','agent.worktree=agent-a6193f5e45a7ae4f1',
        '--label','agent.task=resume-execution-models-narrow-reference',
        '--memory','512m','--cpus','2','--cpuset-cpus','4-7,22-25',
        '-p','127.0.0.1:53345:5432','-e','POSTGRES_PASSWORD=pw',
        '--entrypoint','/usr/bin/nice',image,'-n','19','docker-entrypoint.sh','postgres'])
    created = True
    ready = False
    for attempt in range(30):
        p = subprocess.run(['docker','exec',name,'pg_isready','-U','postgres'], stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if p.returncode == 0:
            ready = True
            break
        time.sleep(1)
    assert ready, 'reference never ready'
    info = json.loads(run('inspect', ['docker','inspect',name]))[0]
    pid = info['State']['Pid']
    assert set(os.sched_getaffinity(pid)) <= {4,5,6,7,22,23,24,25}
    assert os.getpriority(os.PRIO_PROCESS,pid) == 19
    version = run('version', ['docker','exec',name,'psql','-U','postgres','-At','-c','SELECT version()'])
    assert 'PostgreSQL 18.6 ' in version, version
    # The new runtime fixture is the only addition; engine production is still0595.
    fixture = wt / 'crates/pg-engine/tests/sql/a167_subplan_weighted.sql'
    assert fixture.read_bytes() == (base / 'a167_subplan_weighted.sql').read_bytes()
    binary = Path('/home/meszmate/.cache/a167/target/debug/deps/golden-9749ad749af5ca1c')
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert digest == 'e8427752e14282685c6ed68163fe4b21e705ca0bcf61f84073dcf1854dcb3eca'
    (out / 'binary.json').write_text(json.dumps(dict(path=str(binary), sha256=digest,
        compiled_source='0595bf7325b0b4cde8d02d9f8e51ebd17b528d05',
        runtime_fixture='a167_subplan_weighted.sql'), indent=2))
    log = run('weighted-record', [str(binary),'golden','--exact','--nocapture'],
        env=dict(GOLDEN_RECORD='127.0.0.1:53345',GOLDEN_PASSWORD='pw',GOLDEN_FILTER='a167_subplan_weighted'))
    assert re.search(r'^test result: ok\. 1 passed;',log,re.M)
    for suffix in ['sql','out']:
        p = fixture.with_suffix('.' + suffix)
        (out / p.name).write_bytes(p.read_bytes())
    sql = (base / 'a170_vacuum_reference.sql').read_text()
    (out / 'vacuum.sql').write_text(sql)
    log = run('vacuum-reference', ['docker','exec','-i',name,'psql','-U','postgres','-At','-v','ON_ERROR_STOP=1'],data=sql)
    assert log.count('|6000|') == 3, log
    for table,expected in [('ascii2800','index row size 2816'),('ascii8300','index row requires 8320')]:
        log = run('include-' + table, ['docker','exec',name,'psql','-U','postgres','-v','ON_ERROR_STOP=1','-v','VERBOSITY=verbose',
            '-c',f'CREATE INDEX ON vt_{table} (k) INCLUDE (payload)'],want=1)
        assert expected in log and '54000' in log, log
finally:
    if created:
        run('cleanup', ['docker','rm','-f',name])
print('REFERENCE_COMPLETE_NO_PERFORMANCE_CLAIM',flush=True)
