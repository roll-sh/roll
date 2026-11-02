"""Export bounded fetch evidence and a compressed copy of nonbinary frozen inputs."""
from pathlib import Path
import gzip
import hashlib
import io
import json
import shutil
import tarfile

ROOT = Path(__file__).resolve().parent
REPO = Path('/home/meszmate/worktrees/roll/t3-d4020b98')
OUT = REPO/'crates/pg-engine/tools/bench/results/2026-11-02-fetch-visibility'
BASE = Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102')

def sha(path):
    return hashlib.file_digest(path.open('rb'), 'sha256').hexdigest()

def copy(path, relative):
    dest = OUT/relative
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(path, dest)

def compressed(path, relative):
    dest = OUT/relative
    dest.parent.mkdir(parents=True, exist_ok=True)
    with path.open('rb') as source, dest.open('wb') as target:
        with gzip.GzipFile(filename='', fileobj=target, mode='wb', mtime=0) as z:
            shutil.copyfileobj(source, z)

def export():
    OUT.mkdir(exist_ok=False)
    for path in sorted(ROOT.rglob('*')):
        if not path.is_file():
            continue
        rel = path.relative_to(ROOT)
        if 'pgr' in rel.parts or '__pycache__' in rel.parts:
            continue
        if path.name == 'host-cpu-observation.jsonl' or path.name in ['fmt-r1.log', 'fmt-focused-r1.log']:
            compressed(path, str(rel)+'.gz')
        elif path.suffix in ['.py', '.json', '.md', '.rs', '.patch', '.diff', '.log', '.tsv', '.txt', '.stderr']:
            copy(path, rel)
    for name in ['manifest.json', 'lib-pg_engine.json']:
        copy(ROOT.parent/'gather-lifecycle/baseline-bin'/name, Path('baseline-bin')/name)
    for name in ['REPORT.md', 'results.tsv']:
        copy(BASE/'final-regress-5b32046d-release/report'/name, Path('baseline-regression')/name)
    copy(BASE/'final-gates/clippy.log', 'baseline-clippy.log')
    for name in ['README.md', 'postgres.txt', 'gates.json', 'result.json', 'network-preflight.json']:
        copy(REPO/'crates/rolld/tests/results/2026-11-02-service'/name, Path('baseline-service')/name)
    copy(REPO/'crates/pg-engine/ACCEPTANCE.md', 'baseline-service/ACCEPTANCE.md')
    trail = Path('/home/meszmate/.cache/pg-engine-sol-resume/decisions.tsv').read_text().splitlines()
    rows = [line for line in trail[1:] if line.split('\t')[0] >= '2026-11-02T20:42:55Z']
    (OUT/'decisions.tsv').write_text('\n'.join([trail[0]]+rows)+'\n')

    frame = json.loads((ROOT/'frame.json').read_text())
    archived, omitted = {}, {}
    with (OUT/'frozen-inputs.tar.gz').open('wb') as raw:
        with gzip.GzipFile(filename='', fileobj=raw, mode='wb', mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode='w') as archive:
                for i, (name, digest) in enumerate(sorted(frame['files_sha256'].items())):
                    path = Path(name)
                    if sha(path) != digest:
                        raise RuntimeError('frozen input changed: '+name)
                    if path.name in ['roll-engine', 'bench'] or path.name.startswith('pg_regress-'):
                        omitted[name] = {'sha256': digest, 'bytes': path.stat().st_size,
                                         'reason': 'Local retained binary; excluded from portable evidence.'}
                        continue
                    member = 'inputs/'+str(i).zfill(4)+'/'+path.name
                    data = path.read_bytes()
                    info = tarfile.TarInfo(member)
                    info.size = len(data)
                    info.mode = 0o644
                    archive.addfile(info, io.BytesIO(data))
                    archived[name] = {'sha256': digest, 'archive_member': member}
    (OUT/'frozen-input-map.json').write_text(json.dumps({
        'archived': archived, 'omitted': omitted,
        'limit': 'The map checks saved frame bytes offline. Omitted binaries and original database files '
                 'are not independently rehashable from this export.'}, indent=2)+'\n')
    post_frame = ['qualify_source_gates.py', 'cleanup_measurements.py',
                  'cleanup_source_gates.py', 'verify.py', 'export_checkpoint.py']
    if any(str(ROOT/name) in frame['files_sha256'] for name in post_frame):
        raise RuntimeError('post-frame labeling conflicts with the frame')
    (OUT/'post-frame-artifacts.json').write_text(json.dumps({
        'scripts_sha256': {name: sha(OUT/name) for name in post_frame},
        'scope': 'Written after the frame for reporting, cleanup and offline audit; not prospective gates.',
        'superseded_patch': 'source-r1.patch',
        'trial_patch': 'candidate-source/tracked.patch'}, indent=2)+'\n')
    print(json.dumps({'output': str(OUT), 'archived_frozen_inputs': len(archived),
                      'omitted_frozen_binaries': len(omitted)}, indent=2))

if __name__ == '__main__':
    export()
