#!/usr/bin/env python3
import pathlib,os,subprocess,json,hashlib,datetime,re,sys
ROOT=pathlib.Path(__file__).resolve().parent
SRC=ROOT/'green-src'
MANIFEST=ROOT/'green-source-manifest.json'
def stamp():return datetime.datetime.now(datetime.timezone.utc).isoformat()
def verify():
 m=json.loads(MANIFEST.read_text())
 for f in m['files']:
  data=(SRC/f['path']).read_bytes()
  assert len(data)==f['bytes'] and hashlib.sha256(data).hexdigest()==f['sha256'],f['path']
 return {'commit':m['commit'],'files':len(m['files']),'all_match':True,'manifest_sha256':hashlib.sha256(MANIFEST.read_bytes()).hexdigest()}
def write(name,data):(ROOT/name).write_text(json.dumps(data,indent=2)+'\n')
assert os.sched_getaffinity(0)=={4,5,6,7,22,23,24,25},os.sched_getaffinity(0)
assert os.getpriority(os.PRIO_PROCESS,0)==19
assert os.environ['CARGO_BUILD_JOBS']=='2'
assert os.environ['RUST_TEST_THREADS']=='2'
assert os.environ['CARGO_TARGET_DIR']=='/home/meszmate/.cache/a170/target'
write('candidate2-start.json',{'time':stamp(),'source':verify(),'pid':os.getpid(),'affinity':sorted(os.sched_getaffinity(0)),'nice':os.getpriority(os.PRIO_PROCESS,0),'environment':{k:os.environ.get(k) for k in ['CARGO_TARGET_DIR','CARGO_BUILD_JOBS','RUST_TEST_THREADS','CARGO_PROFILE_DEV_DEBUG','CARGO_PROFILE_TEST_DEBUG','RUSTFLAGS','TMPDIR']},'scope':'Candidate combined worker proof only; command result needs raw assessment and separate gates.'})
fresh_file=SRC/'crates/pg-engine/src/lib.rs'
before=fresh_file.stat().st_mtime_ns
os.utime(fresh_file,None)
write('candidate2-freshness.json',{'file':str(fresh_file),'before_mtime_ns':before,'after_mtime_ns':fresh_file.stat().st_mtime_ns,'source_bytes_unchanged':verify()})
cmd=['cargo','test','-v','-p','pg-engine','--lib','combined_index_build_validation','--','--nocapture','--test-threads=1']
write('candidate2-command.json',{'argv':cmd,'cwd':str(SRC),'started':stamp()})
with (ROOT/'candidate2.log').open('wb') as log:
 try:r=subprocess.run(cmd,cwd=SRC,stdout=log,stderr=subprocess.STDOUT,timeout=2700);code=r.returncode
 except subprocess.TimeoutExpired:code=124
raw=(ROOT/'candidate2.log').read_bytes();txt=raw.decode(errors='replace')
bins=[]
for path in re.findall(r'Running unittests src/lib.rs \(([^)]+)\)',txt):
 p=pathlib.Path(path)
 if not p.is_absolute():p=SRC/p
 if p.is_file():
  dest=ROOT/('candidate2-binary-'+p.name);dest.write_bytes(p.read_bytes());dest.chmod(0o755)
  bins.append({'path':str(p),'preserved':str(dest),'sha256':hashlib.sha256(dest.read_bytes()).hexdigest(),'bytes':dest.stat().st_size})
summaries=re.findall(r'test result: ([^\n]+)',txt)
compiled_here='Compiling pg-engine v0.1.0 ('+str(SRC/'crates/pg-engine')+')' in txt
for b in bins:
 assert b['sha256'] != '366b058ab4d004de4ad1bdc33bb5874a56bcfd3ac2701731d383c7cb60974476', 'stale red binary reused'
assert compiled_here, 'candidate was not compiled from intended snapshot'
result={'compiled_from_intended_snapshot':compiled_here,'finished':stamp(),'command_exit':code,'source_after':verify(),'raw_sha256':hashlib.sha256(raw).hexdigest(),'binary':bins,'test_summaries':summaries,'assessment':'UNASSESSED; raw compile/test result must be inspected; focused proof is not full acceptance'}
write('candidate2-result.json',result)
print(json.dumps(result,indent=2),flush=True)
sys.exit(code)
