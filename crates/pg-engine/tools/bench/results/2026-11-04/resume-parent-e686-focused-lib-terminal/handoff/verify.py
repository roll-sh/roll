import hashlib,json,subprocess
from pathlib import Path
scratch=Path('/tmp/combined-index-models-d62e3d3b/encoding-plain-r3')
parent='cace40de4b9df2af42b08027138d9830d17abe9f'
e25='e25c258e759feb28ccb247fa8b10cc25bc7e5810'
red='b56ed0fb4c9a6113cfef09504e2b695a1cf17b70'
base='fe7b0023c9df19a6dae1e6067ba8454998db48c5'
def git(*args): return subprocess.check_output(['git',*args])
def blob(ref,p): return git('show',ref+':'+p)
def sha(b): return hashlib.sha256(b).hexdigest()
def save(name,data): (scratch/name).write_text(json.dumps(data,indent=2)+'\n')
head=git('rev-parse','HEAD').decode().strip()
assert git('rev-parse',head+'^').decode().strip()==parent
assert not git('status','--porcelain')
p='crates/pg-engine/src/exec/sizing/kept.rs'
assert git('diff','--name-only',parent,head).decode().splitlines()==[p]
marker=b'#[cfg(test)]\nmod tests {'
a=blob(parent,p); b=blob(head,p)
assert a[:a.index(marker)]==b[:b.index(marker)]
# The complete scoped diff was inspected; only this cfg(test) file region changes.
old_artifacts={}
for dirname in ['source-r1-successor','proof-r1-successor','encoding-boundary-successor']:
    for f in (scratch.parent/dirname).glob('*.json'):
        old_artifacts[str(f)]=sha(f.read_bytes())
# Validate old manifests against their immutable commits without rewriting them.
for previous in ['source-r1-successor/candidate-manifest.json','proof-r1-successor/red-manifest.json','encoding-boundary-successor/candidate-manifest.json','encoding-boundary-successor/red-manifest.json']:
    m=json.loads((scratch.parent/previous).read_text())
    for item in m['files']:
        data=blob(m['head'],item['path'])
        assert sha(data)==item['sha256'] and len(data)==item['bytes']
files=[]
for path in git('diff','--name-only',base,head).decode().splitlines():
    data=blob(head,path); assert Path(path).read_bytes()==data
    files.append({'path':path,'blob':git('rev-parse',head+':'+path).decode().strip(),'sha256':sha(data),'bytes':len(data)})
refs={
'refs/heads/wip/combined-index-models-d62e3d3b':'4e588363d065aa9f12dc1b987d22dbf6742dcb9f',
'refs/heads/wip/combined-index-models-source-r1-d62e3d3b':e25,
'refs/heads/wip/combined-index-models-red-proof-r1-d62e3d3b':'b43e47a5ba581ac72254d15f39bf7ae4389f35b3',
'refs/heads/wip/combined-index-models-red-proof-r2-d62e3d3b':red,
'refs/heads/wip/combined-index-models-encoding-proof-r2-d62e3d3b':parent,
'refs/heads/wip/combined-index-models-encoding-plain-r3-d62e3d3b':head,
}
raw=git('ls-remote','--heads','origin',*refs).decode()
actual={line.split()[1]:line.split()[0] for line in raw.splitlines()}
assert actual==refs
(scratch/'remote-shas.txt').write_text(raw)
for path,digest in old_artifacts.items(): assert sha(Path(path).read_bytes())==digest
save('preserved-artifact-hashes.json',old_artifacts)
save('candidate-manifest.json',{'status':'UNRUN WIP successor; no source/performance acceptance','head':head,'tree':git('rev-parse',head+'^{tree}').decode().strip(),'parent':parent,'branch':git('branch','--show-current').decode().strip(),'base':base,'production_bytes_match_parent':True,'files':files})
save('verification.json',{'status':'Static byte/Git checks only; no Rust execution','head':head,'parent':parent,'changed_paths':[p],'scope':'Only existing cfg(test) test body/comment; every production byte matches cace','production_prefix_sha256':sha(b[:b.index(marker)]),'prior_manifests_verified_and_preserved':True,'refs_exactly_verified':refs,'diffstat':git('diff','--stat',parent,head).decode()})
(scratch/'from-cace.patch').write_bytes(git('diff','--binary',parent,head))
(scratch/'diffstat.txt').write_bytes(git('diff','--stat',parent,head))
print('Candidate:',head,'tree:',git('rev-parse',head+'^{tree}').decode().strip())
print('Verified',len(files),'committed changed-file blobs, all prior manifests, six exact remote SHAs, and unchanged production bytes.')
