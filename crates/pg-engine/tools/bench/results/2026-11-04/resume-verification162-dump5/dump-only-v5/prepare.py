#!/usr/bin/env python3
from pathlib import Path
import hashlib,json,shutil,subprocess
v=Path(__file__).resolve().parent;old=v.parent/'parity-v4';c=Path('/home/meszmate/.cache/a162');w='/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc'
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
head=subprocess.check_output(['git','-C',w,'rev-parse','HEAD'],text=True).strip()
assert head=='3997cf00a2a301fed65f00469625ff14f223f227'
assert not subprocess.check_output(['git','-C',w,'status','--porcelain'],text=True).strip()
prior=json.loads((old/'source-and-fixture-identity.json').read_text())
assert prior['tested_source']==head
assert prior['engine_sha256']=='9e876733e089594f98399bf3caebb1c2027a1a1506e606e0e6586841b813ce2d'
engine=Path(prior['engine_binary']);assert sha(engine)==prior['engine_sha256']
root=c/'pgdump/resume-3997cf00-dump5';assert not root.exists();(root/'tap').mkdir(parents=True)
fixture_root=c/'pgr/resume-3997cf00-r4'
for rel,digest in json.loads((old/'fixtures-manifest.json').read_text())['pgr'].items():
 p=fixture_root/'src-repo'/rel
 assert not p.is_symlink() and p.resolve().is_relative_to(fixture_root.resolve()),str(p)
 assert sha(p)==digest,str(p)
# Verify exact canonical helper bytes already landed, without moving pinned HEAD.
canonical=subprocess.check_output(['git','-C',w,'show','d599cf05:crates/pg-engine/tools/audit-tap.py'])
assert canonical==(v.parent/'audit-tap.py').read_bytes()
(v/'audit-tap.py').write_bytes(canonical)
shutil.copyfile('/home/meszmate/.cache/pg-engine-resume-20261104/storage_correctness/check-tap.py',v/'check-tap.py')
shutil.copyfile(v.parent/'tap-helper-parent-audit.json',v/'tap-helper-parent-audit.json')
sealed={str(p.relative_to(old)):sha(p) for p in sorted(old.rglob('*')) if p.is_file()}
(v/'v4-sealed-hashes.json').write_text(json.dumps(sealed,indent=2)+'\n')
(v/'prior-source-identity.json').write_text(json.dumps(prior,indent=2)+'\n')
record={'tested_head':head,'engine_binary':str(engine),'engine_sha256':sha(engine),'fixture_root':str(fixture_root/'src-repo'),'fixture_source_commit':prior['fixtures']['pgr']['source_commit'],'fixture_tag':'REL_18_6','fixture_files_checked':len(json.loads((old/'fixtures-manifest.json').read_text())['pgr']),'dump_root':str(root),'audit_tap_sha256':sha(v/'audit-tap.py'),'count_checker_sha256':sha(v/'check-tap.py'),'sealed_v4_files':len(sealed),'runner_sha256':sha(v/'check-dump-only.sh'),'full_service':'HELD'}
(v/'source-identity.json').write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(record,indent=2))
