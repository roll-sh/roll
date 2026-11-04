#!/usr/bin/env python3
"""Owned, bounded PG18.6 reference only; no performance measurement."""
import hashlib, json, os, re, subprocess, time
from pathlib import Path

base = Path('/home/meszmate/.cache/pg-engine-resume-20261104/execution_models')
wt = Path('/home/meszmate/roll/.claude/worktrees/agent-a6193f5e45a7ae4f1')
out = base / 'a167170-r3-reference'
out.mkdir(exist_ok=False)
name = 'a167170-r3-reference'
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
terminal = (base / 'a167-weighted-final-launch.log').read_text()
assert 'HEAVY-EXIT ' in terminal, '167 pin not terminal; no new reference allowed'
assert not run('existing-owned', ['docker','ps','-aq','--filter','name=^/' + name + '$']).strip()
run('image', ['docker','image','inspect',image])
created = False
try:
    run('create', ['docker','run','--rm','-d','--name',name,
        '--label','agent.worktree=agent-a6193f5e45a7ae4f1',
        '--label','agent.task=resume-execution-models-narrow-reference',
        '--memory','512m','--cpus','2','--cpuset-cpus','4-7,22-25',
        '-p','127.0.0.1:53345:5432','-e','POSTGRES_PASSWORD=pw',
        '--entrypoint','/bin/nice',image,'-n','19','docker-entrypoint.sh','postgres'])
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
    source = {}
    for owner, path in [('167',wt),('170',Path('/home/meszmate/roll/.claude/worktrees/agent-a0718617fe2ff9006'))]:
        source[owner] = {k: subprocess.check_output(['git','-C',str(path),*cmd],text=True).strip() for k,cmd in [('head',['rev-parse','HEAD']),('tree',['rev-parse','HEAD^{tree}']),('status',['status','--porcelain=v1'])]}
    (out/'owner-sources.json').write_text(json.dumps(source,indent=2))
    fixture = wt/'crates/pg-engine/tests/sql/a167_subplan_owned.sql'
    assert fixture.read_bytes() == (base/'a167_subplan_owned.sql').read_bytes()
    binary = Path('/home/meszmate/.cache/a167/target/debug/deps/golden-9749ad749af5ca1c')
    assert not subprocess.check_output(['git','-C',str(wt),'diff','278a28966a8921da90c6ead9cfeb3f332306d788','HEAD','--','crates/pg-engine/src','Cargo.toml','Cargo.lock','crates/pg-engine/Cargo.toml'])
    debug=next(c for c in json.loads((base/'a167-weighted-final/checks.json').read_text()) if c['name']=='debug')
    assert debug['pass'] and hashlib.sha256(binary.read_bytes()).hexdigest()==debug['binaries'][str(binary)]['sha256']
    (out/'binary.json').write_text(json.dumps(dict(path=str(binary),sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),compiled_source='278a28966a8921da90c6ead9cfeb3f332306d788',limitation='Runtime fixture added after pinned production build; Rust/Cargo identity checked.'),indent=2))
    log = run('owned-record',[str(binary),'golden','--exact','--nocapture'],env=dict(GOLDEN_RECORD='127.0.0.1:53345',GOLDEN_PASSWORD='pw',GOLDEN_FILTER='a167_subplan_owned'))
    assert re.search(r'^test result: ok\. 1 passed;',log,re.M)
    for suffix in ['sql','out']:
        p=fixture.with_suffix('.'+suffix)
        (out/p.name).write_bytes(p.read_bytes())
    # Plain psql reference separately retains actual layouts and COPY values.
    sql=(base/'a170_r3_reference.sql').read_text()
    (out/'a170_r3_reference.sql').write_text(sql)
    log=run('drop-copy-reference',['docker','exec','-i',name,'psql','-U','postgres','-At','-v','ON_ERROR_STOP=1'],data=sql)
    assert 'drop-before|' in log and 'drop-after|' in log and 'copy-five|' in log and 'copy-main|' in log,log
    # Recreate only tiny a167 tables for the optional read-only serial fallback shape.
    setup="CREATE TABLE a167_o(k int); INSERT INTO a167_o SELECT generate_series(1,6); CREATE TABLE a167_n(nk int,rk int); INSERT INTO a167_n SELECT g,g%3+1 FROM generate_series(1,9)g; ANALYZE a167_o,a167_n;\n"
    shape=(base/'a167_material_fallback.sql').read_text()
    (out/'a167_material_fallback.sql').write_text(shape)
    run('material-fallback-shape',['docker','exec','-i',name,'psql','-U','postgres','-At','-v','ON_ERROR_STOP=1'],data=setup+shape)
finally:
    if created:
        run('cleanup', ['docker','rm','-f',name])
print('REFERENCE_COMPLETE_NO_PERFORMANCE_CLAIM',flush=True)
