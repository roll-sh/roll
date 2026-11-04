#!/usr/bin/env python3
from pathlib import Path
import json,subprocess
v=Path(__file__).resolve().parent;c=Path('/home/meszmate/.cache/a162')
records=[]
def run(name,cmd):
 r=subprocess.run(cmd,capture_output=True,text=True)
 (v/(name+'.stdout.log')).write_text(r.stdout);(v/(name+'.stderr.log')).write_text(r.stderr)
 records.append({'name':name,'command':cmd,'exit':r.returncode})
 (v/'preflight-exits.json').write_text(json.dumps(records,indent=2)+'\n')
 assert r.returncode==0,(name,r.returncode,r.stderr)
 return r.stdout
for client,suite,cwd in [('a162-psql','pgr','build'),('a162-dclient','pgdump','tap')]:
 root=c/suite/'resume-3997cf00-r4'
 data=json.loads(run(client+'-inspect',['docker','inspect','--format','{{json .Mounts}}',client]))
 assert any(m['Type']=='bind' and root.is_relative_to(Path(m['Destination'])) and m['Source']==m['Destination'] and m['RW'] for m in data),client
 output=run(client+'-cwd',['docker','exec','-w',str(root/cwd),client,'sh','-eu','-c','pwd; test -w .; psql --version; pg_dump --version; pg_restore --version'])
 assert str(root/cwd) in output and '18.6' in output
run('reference-ready',['docker','exec','a162-rref','pg_isready','-U','postgres'])
run('reference-version',['docker','exec','a162-rref','psql','-X','-At','-U','postgres','-d','postgres','-c','SELECT version()'])
run('isolation-reference',['docker','exec','-e','PGPASSWORD=pw','a162-iref','psql','-X','-At','-h','127.0.0.1','-p','53250','-U','postgres','-d','postgres','-c','SHOW server_version'])
print('PASS: bind mounts, actual writable client workdirs and PostgreSQL18.6 clients/references')
