#!/usr/bin/env python3
from pathlib import Path
import hashlib,json,re,subprocess
v=Path(__file__).resolve().parent;i=json.loads((v/'source-identity.json').read_text());w='/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc'
assert subprocess.check_output(['git','-C',w,'rev-parse','HEAD'],text=True).strip()==i['tested_head']
assert not subprocess.check_output(['git','-C',w,'status','--porcelain'],text=True).strip()
engine=Path(i['dump_root'])/'roll-engine.bin';actual=hashlib.sha256(engine.read_bytes()).hexdigest();assert actual==i['engine_sha256']
for rel,digest in json.loads((v/'v4-sealed-hashes.json').read_text()).items():assert hashlib.sha256((v.parent/'parity-v4'/rel).read_bytes()).hexdigest()==digest,rel
for rel,digest in json.loads((v.parent/'parity-v4/v3-sealed-hashes.json').read_text()).items():assert hashlib.sha256((v.parent/rel).read_bytes()).hexdigest()==digest,rel
log=(v/'dump.log').read_text();assert re.search(r'^test result: ok\. 1 passed; 0 failed;',log,re.M)
assert not re.search(r'^test result: FAILED|^error:',log,re.M)
count=json.loads((Path(i['dump_root'])/'outcome.json').read_text());assert count['passed']==14817 and count['failed']==0
plan=json.loads((v/'dump-audit.log').read_text());assert plan['status']=='PASS'
result={'status':'PASS','tested_head':i['tested_head'],'actual_dump_binary':str(engine),'actual_dump_binary_sha256':actual,'rust_summary':'1 passed;0 failed','tap_count':{'passed':14817,'failed':0},'complete_plan_audit':'PASS','sealed_v4_unchanged':True,'sealed_v3_unchanged':True,'full_service':'HELD','scope':'remaining strictdump gate only; final combined service acceptance still pending'}
(v/'final-audit.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
