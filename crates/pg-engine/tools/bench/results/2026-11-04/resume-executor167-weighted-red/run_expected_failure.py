#!/usr/bin/env python3
"""Pin source and preserve a single requested red; never label source passing."""
import hashlib, json, os, re, subprocess, sys
from pathlib import Path
p = json.loads(Path(sys.argv[1]).read_text())
wt = p['worktree']; out = Path(p['output']); out.mkdir(exist_ok=False)
def git(*args):
    return subprocess.check_output(['git','-C',wt,*args],text=True).strip()
def source():
    return dict(head=git('rev-parse','HEAD'),tree=git('rev-parse','HEAD^{tree}'),
        status=git('status','--porcelain=v1'),patch_sha256=hashlib.sha256(git('diff','--binary','HEAD').encode()).hexdigest())
pin=source(); assert pin['head']==p['head'] and not pin['status'],pin
assert os.getpriority(os.PRIO_PROCESS,0)==19
assert set(os.sched_getaffinity(0)) <= {4,5,6,7,22,23,24,25}
(out/'source.json').write_text(json.dumps(pin,indent=2))
(out/'execution.json').write_text(json.dumps(dict(nice=19,effective_cpus=sorted(os.sched_getaffinity(0)),command=p['command'],env=p.get('env',{})),indent=2))
binary=p.get('binary')
if binary:
    digest=hashlib.sha256(Path(binary).read_bytes()).hexdigest()
    assert digest==p['binary_sha256']
    assert not git('diff',p['binary_source'],'HEAD','--','crates/pg-engine/src','Cargo.toml','Cargo.lock','crates/pg-engine/Cargo.toml')
    (out/'binary.json').write_text(json.dumps(dict(path=binary,sha256=digest,compiled_source=p['binary_source'],limitation='New runtime SQL golden added after compiled source; production Rust/Cargo verified identical.'),indent=2))
with (out/'raw.log').open('w') as log:
    r=subprocess.run(p['command'],cwd=wt,env=dict(os.environ,**p.get('env',{})),stdout=log,stderr=subprocess.STDOUT)
raw=(out/'raw.log').read_text();binaries={}
for b in re.findall(r'Running .+ \((/[^)]+)\)',raw):
    f=Path(b);binaries[b]=dict(sha256=hashlib.sha256(f.read_bytes()).hexdigest(),bytes=f.stat().st_size)
result=dict(exit=r.returncode,expected_exit=101,source_after=source(),source_status='FAILED',
    test_summaries=re.findall(r'^test result:.*$',raw,re.M),binaries=binaries,
    raw_sha256=hashlib.sha256((out/'raw.log').read_bytes()).hexdigest(),
    distinguishing_pattern_present=bool(re.search(p['pattern'],raw,re.M)))
(out/'result.json').write_text(json.dumps(result,indent=2))
print(json.dumps(result),flush=True)
assert result['source_after']==pin
assert r.returncode==101 and result['distinguishing_pattern_present'] and any('FAILED' in x for x in result['test_summaries'])
print('EXPECTED_RED_PRESERVED_SOURCE_REMAINS_FAILED',flush=True)
