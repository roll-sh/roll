"""Record the exact matched release build; run only after build completion."""
from pathlib import Path
import hashlib,json,subprocess,datetime,shutil
ROOT=Path(__file__).resolve().parent
REPO=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
BASE=ROOT.parent/'gather-lifecycle/baseline-bin'
def check(x,m):
 if not x:raise RuntimeError(m)
def sha(p):return hashlib.file_digest(Path(p).open('rb'),'sha256').hexdigest()
def save(p,v):
 with p.open('x') as f:json.dump(v,f,indent=2);f.write('\n')
paths=['crates/pg-engine/src/exec/sizing.rs','crates/pg-engine/tests/wire.rs']
changed=subprocess.check_output(['git','diff','--name-only'],cwd=REPO,text=True).splitlines()
check(sorted(changed)==sorted(paths),'unexpected changed files')
source={p:sha(REPO/p) for p in paths}
head=subprocess.check_output(['git','rev-parse','HEAD'],cwd=REPO,text=True).strip()
out=ROOT/'candidate-source';out.mkdir(exist_ok=False)
for p in paths:
 dest=out/p;dest.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(REPO/p,dest)
patch=subprocess.check_output(['git','diff','--binary'],cwd=REPO)
(out/'tracked.patch').write_bytes(patch)
lib=REPO/'target/release/.fingerprint/pg-engine-b90198ef267a1eed/lib-pg_engine.json'
a=json.loads((BASE/'lib-pg_engine.json').read_text());b=json.loads(lib.read_text())
check(a==b,'dependency fingerprints/profile/compiler/features differ')
save(ROOT/'matched-dependencies.json',{'all_fields_equal':True,'dependency_count':len(a['deps']),'baseline':a,'candidate':b})
bin=ROOT/'candidate-bin';bin.mkdir(exist_ok=False)
shutil.copy2(REPO/'target/release/roll-engine',bin/'roll-engine');shutil.copy2(lib,bin/'lib-pg_engine.json')
manifest={'at_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'source':head+'+'+sha(out/'tracked.patch'),'source_identity_kind':'HEAD plus SHA256 of tracked binary diff covering both modified files','source_sha256':source,'binary_sha256':sha(bin/'roll-engine'),'rustc':subprocess.check_output(['rustc','-vV'],text=True),'build_command':['cargo','build','-p','pg-engine','--release','--bin','roll-engine','--example','bench','--example','scan_bench'],'environment':{'CARGO_PROFILE_RELEASE_DEBUG':'line-tables-only'},'dependency_fingerprint_matches_baseline':True,'dependency_count':len(a['deps']),'release_build_log_sha256':sha(ROOT/'release-build-r1.log')}
save(bin/'manifest.json',manifest)
print(json.dumps(manifest,indent=2))
