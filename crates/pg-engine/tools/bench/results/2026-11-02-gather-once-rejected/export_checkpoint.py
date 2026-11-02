"""Copy bounded rejection evidence, preserving original raw logs and frozen sources."""
from pathlib import Path
import gzip,json,shutil
ROOT=Path(__file__).resolve().parent
REPO=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
OUT=REPO/'crates/pg-engine/tools/bench/results/2026-11-02-gather-once-rejected'
OUT.mkdir(exist_ok=False)
allowed={'.json','.py','.rs','.md','.patch','.log','.tsv'}
for p in ROOT.rglob('*'):
 if not p.is_file():continue
 rel=p.relative_to(ROOT)
 if 'rejected-target-entrypoints' in rel.parts:continue
 if p.name=='host-cpu-observation.jsonl':
  out=OUT/rel.with_suffix('.jsonl.gz');out.parent.mkdir(parents=True,exist_ok=True)
  with p.open('rb') as src,out.open('xb') as dst:
   with gzip.GzipFile(filename='',mode='wb',fileobj=dst,mtime=0) as z:shutil.copyfileobj(src,z)
 elif p.suffix in allowed:
  out=OUT/rel;out.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(p,out)
profile=Path('/home/meszmate/.cache/pg-engine-perf-20261101/gather-fallback-20261102')
for p in profile.rglob('*'):
 if not p.is_file():continue
 rel=p.relative_to(profile)
 if p.name.endswith('-classified.json') or p.name.endswith('-stacks.txt') or p.name.endswith('-replay.json'):continue
 if p.suffix not in ['.json','.log','.py']:continue
 out=OUT/'profile-fallback'/rel;out.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(p,out)
# Standalone baseline provenance comes from the retained matched artifact.
base=ROOT.parent/'gather-lifecycle/baseline-bin'
for name in ['manifest.json','lib-pg_engine.json']:
 p=OUT/'baseline-bin'/name;p.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(base/name,p)
print(OUT)
