#!/usr/bin/env python3
"""Exercise prior false-success/resource replacement bugs with fake Docker only."""
from pathlib import Path
import json, os, subprocess, tempfile
BASE = Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner')
SCRIPTS = Path('/home/meszmate/.cache/a165/scripts')
results=[]
with tempfile.TemporaryDirectory(prefix='a165-wrapper-selftest-', dir=BASE) as temp:
    root=Path(temp); fake=root/'bin';fake.mkdir()
    docker=fake/'docker'
    docker.write_text('''#!/usr/bin/env python3
import os,sys
from pathlib import Path
with open(os.environ['FAKE_DOCKER_LOG'],'a') as f:f.write(repr(sys.argv[1:])+'\\n')
a=sys.argv[1:]
if a[:2]==['inspect','-f']: print('12345');sys.exit(0)
if a[0]=='inspect':sys.exit(0)
if a[0]=='run':
 print('number of transactions actually processed: 7')
 print('number of failed transactions: 0 (0.000%)')
 print('tps = 7.0 (without initial connection time)')
 sys.exit(0)
if a[0]=='exec':print('injected perf failure',file=sys.stderr);sys.exit(17)
sys.exit(99)
''');docker.chmod(0o755)
    for name in ['ss','sleep']:
        f=fake/name;f.write_text('#!/bin/sh\nexit 0\n');f.chmod(0o755)
    env=dict(os.environ,PATH=str(fake)+':'+os.environ['PATH'])
    for name,args in [
        ('matrix',['matrix.sh',str(root/'matrix.tsv'),'1','pg','base','resume']),
        ('scaled',['conn1000.sh','resume','1000']),
        ('profiler',['prof.sh','resume','select-only','simple','1','1','wrapper-selftest']),
    ]:
        log=root/(name+'.docker.log')
        runenv=dict(env,FAKE_DOCKER_LOG=str(log),MATRIX_DATA=str(root/'matrix-data'),CONN_DATA=str(root/'conn-data'))
        p=subprocess.run(['bash',str(SCRIPTS/args[0]),*args[1:]],env=runenv,capture_output=True,text=True)
        calls=log.read_text() if log.exists() else ''
        assert p.returncode!=0,(name,p.stdout,p.stderr)
        if name!='profiler':
            assert "['rm'" not in calls and "['run'" not in calls,(name,calls)
        else:
            exits=Path('/home/meszmate/.cache/a165/perf/wrapper-selftest.exits.tsv').read_text()
            assert exits=='perf\t17\npgbench\t0\n',exits
        results.append(dict(case=name,exit=p.returncode,calls=calls,stdout=p.stdout,stderr=p.stderr))
(BASE/'wrapper-selftest-results.json').write_text(json.dumps(results,indent=2)+'\n')
print('3 fake-Docker propagation/ownership checks passed; no real Docker calls.')
