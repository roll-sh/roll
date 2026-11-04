#!/usr/bin/env python3
import pathlib,subprocess,os,json,hashlib,re,datetime,sys
ROOT=pathlib.Path(__file__).resolve().parent;SRC=ROOT/'src'
def stamp():return datetime.datetime.now(datetime.timezone.utc).isoformat()
def save(name,v):(ROOT/name).write_text(json.dumps(v,indent=2)+'\n')
def verify():
 m=json.loads((ROOT/'source-manifest.json').read_text())
 for f in m['files']:
  b=(SRC/f['path']).read_bytes();assert len(b)==f['bytes'] and hashlib.sha256(b).hexdigest()==f['sha256'],f['path']
 return {'commit':m['commit'],'files':len(m['files']),'match':True,'manifest_sha256':hashlib.sha256((ROOT/'source-manifest.json').read_bytes()).hexdigest()}
assert os.sched_getaffinity(0)=={4,5,6,7,22,23,24,25}
assert os.getpriority(os.PRIO_PROCESS,0)==19
assert os.environ['CARGO_TARGET_DIR']=='/home/meszmate/.cache/a170/target'
assert os.environ['CARGO_BUILD_JOBS']=='2' and os.environ['RUST_TEST_THREADS']=='2'
save('startup.json',{'time':stamp(),'pid':os.getpid(),'source':verify(),'cpus':sorted(os.sched_getaffinity(0)),'nice':os.getpriority(os.PRIO_PROCESS,0),'jobs':os.environ['CARGO_BUILD_JOBS'],'threads':os.environ['RUST_TEST_THREADS'],'target':os.environ['CARGO_TARGET_DIR']})
f=SRC/'crates/pg-engine/src/lib.rs';before=f.stat().st_mtime_ns;os.utime(f,None);save('freshness.json',{'mtime_before':before,'mtime_after':f.stat().st_mtime_ns,'byte_hashes_after_touch':verify()})
stages=[('combined-focused',['cargo','test','-v','-p','pg-engine','--lib','combined_index','--','--nocapture','--test-threads=2'],[3,0,0]),('full-lib',['cargo','test','-v','-p','pg-engine','--lib','--','--nocapture','--test-threads=2'],[193,0,2])]
prior_hash=None;done=[]
for i,(name,cmd,expected) in enumerate(stages):
 save(name+'-command.json',{'argv':cmd,'cwd':str(SRC),'expected_pass_fail_ignore':expected,'time':stamp(),'source_before':verify()})
 with (ROOT/(name+'.log')).open('wb') as log:
  try:r=subprocess.run(cmd,cwd=SRC,stdout=log,stderr=subprocess.STDOUT,timeout=2700);code=r.returncode
  except subprocess.TimeoutExpired:code=124
 raw=(ROOT/(name+'.log')).read_bytes();txt=raw.decode(errors='replace')
 paths=re.findall(r'Running `(/[^` ]+/pg_engine-[^` ]+)(?: |`)',txt)
 bins=[]
 for bpath in paths:
  p=pathlib.Path(bpath);data=p.read_bytes();digest=hashlib.sha256(data).hexdigest();dest=ROOT/('binary-'+digest)
  if not dest.exists():dest.write_bytes(data);dest.chmod(0o755)
  bins.append({'path':bpath,'preserved':str(dest),'sha256':digest,'bytes':len(data)})
 counts=[list(map(int,m)) for m in re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored',txt)]
 compiled='Compiling pg-engine v0.1.0 ('+str(SRC/'crates/pg-engine')+')' in txt
 valid=len(bins)==1 and (compiled if i==0 else bins[0]['sha256']==prior_hash)
 result={'time':stamp(),'command_exit':code,'source_after':verify(),'raw_sha256':hashlib.sha256(raw).hexdigest(),'counts':counts,'binary':bins,'compiled_from_source_this_stage':compiled,'provenance_valid':valid,'expected':expected,'assessment':'Focused/test-library gate only; no integration/service/PGoracle/performance acceptance'}
 save(name+'-result.json',result);done.append({'stage':name,'command_exit':code,'counts':counts,'provenance_valid':valid});save('progress.json',done)
 if code or counts!=[expected] or not valid:
  save('TERMINAL.json',{'status':'FAILED','stage':name,'raw_command_exit':code,'counts':counts,'expected':expected,'provenance_valid':valid,'later_stages':'NOT RUN','completed':done});sys.exit(code or 2)
 prior_hash=bins[0]['sha256']
save('TERMINAL.json',{'status':'PASS','completed':done,'source':verify(),'acceptance':'Focused and multithreaded library gates only; remaining PGoracle/P6-P12/final integration remain held'})
print(json.dumps({'status':'PASS','completed':done}),flush=True)
