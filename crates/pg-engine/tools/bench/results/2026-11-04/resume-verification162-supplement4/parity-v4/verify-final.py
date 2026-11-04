#!/usr/bin/env python3
from pathlib import Path
import hashlib,json,subprocess
v=Path(__file__).resolve().parent;c=Path('/home/meszmate/.cache/a162');w='/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc'
identity=json.loads((v/'source-and-fixture-identity.json').read_text())
assert subprocess.check_output(['git','-C',w,'rev-parse','HEAD'],text=True).strip()==identity['tested_source']
assert not subprocess.check_output(['git','-C',w,'status','--porcelain'],text=True).strip()
binaries={}
for suite in ['pgr','iso','pgdump']:
 p=c/suite/'resume-3997cf00-r4/roll-engine.bin'
 digest=hashlib.sha256(p.read_bytes()).hexdigest()
 assert digest==identity['engine_sha256'],str(p)
 binaries[str(p)]=digest
for suite,entries in json.loads((v/'fixtures-manifest.json').read_text()).items():
 root=c/suite/'resume-3997cf00-r4';src=root/'src-repo'
 for rel,digest in entries.items():
  p=src/rel;assert p.resolve().is_relative_to(root.resolve()),str(p)
  assert hashlib.sha256(p.read_bytes()).hexdigest()==digest,str(p)
for rel,digest in json.loads((v/'v3-sealed-hashes.json').read_text()).items():
 assert hashlib.sha256((v.parent/rel).read_bytes()).hexdigest()==digest,'sealed v3 changed: '+rel
result={'status':'PASS','source':identity['tested_source'],'actual_tested_binaries':binaries,'fixture_bytes_unchanged':True,'sealed_v3_unchanged':True,'prior_pair_smoke_array':'retained from same pinned source; not rerun','fullservice':'HELD'}
(v/'final-identity-audit.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
