#!/usr/bin/env python3
from pathlib import Path
import json,socket,subprocess
v=Path(__file__).resolve().parent;identity=json.loads((v/'source-identity.json').read_text());root=Path(identity['dump_root']);records=[]
def run(name,command):
 r=subprocess.run(command,capture_output=True,text=True,timeout=30)
 (v/(name+'.stdout.log')).write_text(r.stdout);(v/(name+'.stderr.log')).write_text(r.stderr)
 records.append({'name':name,'command':command,'exit':r.returncode})
 (v/'preflight-exits.json').write_text(json.dumps(records,indent=2)+'\n')
 assert r.returncode==0,(name,r.returncode,r.stderr)
 return r.stdout
mounts=json.loads(run('client-mounts',['docker','inspect','--format','{{json .Mounts}}','a162-dclient']))
assert any(m['Type']=='bind' and m['RW'] and m['Source']==m['Destination'] and root.is_relative_to(Path(m['Destination'])) for m in mounts)
run('client-cwd',['docker','exec','-w',str(root/'tap'),'a162-dclient','sh','-eu','-c','pwd; test -w .'])
for tool in ['pg_dump','pg_restore','psql']:
 output=run(tool+'-version',['docker','exec','-w',str(root/'tap'),'a162-dclient',tool,'--version'])
 # Literal, whole-line exact gate. No regex interpretation or relaxed version threshold.
 r=subprocess.run(['rg','-F','-x','-q',tool+' (PostgreSQL) 18.6'],input=output,capture_output=True,text=True)
 records.append({'name':tool+'-exact-version','command':['rg','-F','-x','-q',tool+' (PostgreSQL) 18.6'],'exit':r.returncode})
 (v/'preflight-exits.json').write_text(json.dumps(records,indent=2)+'\n')
 assert r.returncode==0,(tool,output)
try:
 with socket.create_connection(('127.0.0.1',53255),timeout=.2):raise RuntimeError('dump port already occupied; refusing launch')
except (ConnectionRefusedError,TimeoutError):pass
print('PASS: actual writable bind-mount cwd; exact complete18.6 pg_dump/pg_restore/psql; free owned port')
