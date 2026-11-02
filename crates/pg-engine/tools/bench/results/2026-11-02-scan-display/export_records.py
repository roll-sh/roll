"""Copy reviewable records, excluding databases and executable payloads."""
from pathlib import Path
import gzip
import shutil

ROOT = Path(__file__).resolve().parent
OUT = Path('/home/meszmate/worktrees/roll/t3-d4020b98/crates/pg-engine/tools/bench/results/2026-11-02-scan-display')


def copy(path, relative=None):
    target = OUT / (relative or path.relative_to(ROOT))
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(path, target)


for name in [
    'frame.json', 'candidate-source.json', 'candidate-source.patch',
    'matched-dependencies.json', 'matched-dependencies-final.json',
    'p1-frame.json', 'p1-statistics.json', 'p1-accounting.json',
    'rebuild-identity.json', 'release-build.log', 'release-build-final.log',
    'audit_pairs.py', 'p1_statistics.py', 'measure.py', 'run_required_gates.py',
    'design-review-r1.json', 'design-review-r2.json', 'design-review-r3.json', 'performance-review-r1.json',
    'source-gate-controller.log', 'source-gates-skipped.json',
    'restoration.json', 'restored-build.json', 'restored-build.log',
    'rejected-source.patch', 'run_candidate_regress.py', 'export_records.py',
    'final-live-cleanup.json', 'recheck_bottleneck.py', 'next-bottleneck.json',
    'focused-test.log', 'restored-clippy.log', 'restored-clippy.json',
    'evidence-review-r1.json', 'evidence-review-r2.json', 'review-response.json',
]:
    path = ROOT / name
    if path.is_file():
        copy(path)
for path in ROOT.glob('focused-test-r*.log'):
    copy(path)
for path in ROOT.glob('paired-r*-audit.json'):
    copy(path)
for name in ['candidate-bin', 'candidate-final-bin']:
    copy(ROOT / name / 'manifest.json')
for path in (ROOT / 'source-p1').rglob('*'):
    if path.is_file():
        copy(path)
for name in ['paired-r1', 'paired-r2', 'paired-r3', 'paired-r4', 'paired-p1']:
    root = ROOT / name
    for path in root.iterdir():
        if path.is_file():
            if path.name == 'host-cpu-observation.jsonl':
                target = OUT / path.relative_to(ROOT).with_suffix('.jsonl.gz')
                target.parent.mkdir(parents=True, exist_ok=True)
                with path.open('rb') as src, target.open('wb') as raw:
                    with gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as dest:
                        shutil.copyfileobj(src, dest)
            else:
                copy(path)
    paired = root / 'candidate-vs-baseline'
    if paired.is_dir():
        for path in paired.iterdir():
            if path.is_file():
                copy(path)
gates = ROOT / 'source-gates'
if gates.is_dir():
    for path in gates.iterdir():
        if path.is_file():
            copy(path)
