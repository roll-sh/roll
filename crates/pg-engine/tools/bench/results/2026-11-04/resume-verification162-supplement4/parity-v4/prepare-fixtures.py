#!/usr/bin/env python3
from pathlib import Path
import hashlib,json,shutil,subprocess
v=Path(__file__).resolve().parent;c=Path('/home/meszmate/.cache/a162')
w='/home/meszmate/roll/.claude/worktrees/agent-acb216a1743adeadc'
head=subprocess.check_output(['git','-C',w,'rev-parse','HEAD'],text=True).strip()
assert head=='3997cf00a2a301fed65f00469625ff14f223f227'
assert not subprocess.check_output(['git','-C',w,'status','--porcelain'],text=True).strip()
# Freeze exact failed attempt bytes without writing its files.
old=v.parent
sealed={}
for rel in ['check-parity.sh','parity-status.tsv','parity-job-v3.log','regress.log','regress-report/REPORT.md','regress-report/results.tsv']:
 p=old/rel;sealed[rel]=hashlib.sha256(p.read_bytes()).hexdigest()
(v/'v3-sealed-hashes.json').write_text(json.dumps(sealed,indent=2)+'\n')
shutil.copyfile(old/'parity-status.tsv',v/'parity-status-v3.tsv')
shutil.copyfile(old/'tap-helper-parent-audit.json',v/'tap-helper-parent-audit.json')
manifest={};sources={}
for suite in ['pgr','iso']:
 src=c/suite/'src-repo';root=c/suite/'resume-3997cf00-r4';dst=root/'src-repo'
 assert not root.exists(),f'new owned root already exists: {root}'
 commit=subprocess.check_output(['git','-C',str(src),'rev-parse','HEAD'],text=True).strip()
 tag=subprocess.check_output(['git','-C',str(src),'describe','--tags','--exact-match'],text=True).strip()
 assert tag=='REL_18_6'
 assert not subprocess.check_output(['git','-C',str(src),'status','--porcelain'],text=True).strip()
 shutil.copytree(src,dst,ignore=shutil.ignore_patterns('.git'),symlinks=False)
 entries={}
 for p in sorted(dst.rglob('*')):
  assert not p.is_symlink(),str(p)
  assert p.resolve().is_relative_to(root.resolve()),str(p)
  if p.is_file():
   rel=p.relative_to(dst);actual=hashlib.sha256(p.read_bytes()).hexdigest()
   assert actual==hashlib.sha256((src/rel).read_bytes()).hexdigest(),str(rel)
   entries[str(rel)]=actual
 manifest[suite]=entries;sources[suite]={'original':str(src),'destination':str(dst),'source_commit':commit,'tag':tag,'files':len(entries),'canonical_paths_within_allowed_root':True}
 root.joinpath('build').mkdir()
d=c/'pgdump/resume-3997cf00-r4';assert not d.exists();d.joinpath('tap').mkdir(parents=True)
binpath=c/'target/release/roll-engine';binhash=hashlib.sha256(binpath.read_bytes()).hexdigest()
pinned=c/'pgr/resume-3997cf00-r4/roll-engine-pinned.bin';shutil.copyfile(binpath,pinned);pinned.chmod(0o755)
assert hashlib.sha256(pinned.read_bytes()).hexdigest()==binhash
(v/'fixtures-manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
record={'tested_source':head,'engine_binary':str(pinned),'engine_sha256':binhash,'engine_version':subprocess.check_output([str(pinned),'--version'],text=True).strip(),'build_commands':'same clean3997cf00 release build and subsequent release dtbuild recorded in parent status.tsv; cargo may relink; this exact binary copied before corrected checks','fixtures':sources}
(v/'source-and-fixture-identity.json').write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(record,indent=2))
