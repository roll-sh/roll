#!/usr/bin/env python3
from pathlib import Path
import json,subprocess
v=Path(__file__).resolve().parent
out=v/'version-matcher-proof';out.mkdir(exist_ok=True)
raw=(v/'dump-preflight.log').read_text()
records=[]
for label,literal,text,expected in [('original-regex-real',False,raw,1),('literal-real',True,raw,0),('literal-wrong-version',True,raw.replace('18.6','18.5'),1),('literal-missing-client',True,'pg_restore (PostgreSQL) 18.6\n',1)]:
 p=out/(label+'.log');p.write_text(text)
 cmd=['rg']+(['-F'] if literal else [])+['-q','pg_dump (PostgreSQL) 18.6',str(p)]
 r=subprocess.run(cmd,capture_output=True,text=True)
 (out/(label+'.stdout.log')).write_text(r.stdout);(out/(label+'.stderr.log')).write_text(r.stderr)
 record={'case':label,'command':cmd,'exit':r.returncode,'expected_exit':expected};records.append(record)
 assert r.returncode==expected,record
(out/'proof.json').write_text(json.dumps({'status':'PASS','scope':'saved bytes only; no service or source suite','cases':records},indent=2)+'\n')
print(json.dumps(records,indent=2))
