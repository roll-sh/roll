"""Offline decode retained CPU samples against each original ELF mapping."""
from pathlib import Path
import hashlib,json,subprocess,signal,sys
R=Path('/home/meszmate/.cache/pg-engine-perf-20261101/gather-fallback-20261102')
P=Path('/home/meszmate/.cache/pg-engine-perf-20261101/planning-cpu')
PROFILER='813b04eed73f'
def call(args):return subprocess.check_output([str(x) for x in args],text=True,timeout=180).strip()
def check(b,s):
 if not b:raise RuntimeError(s)
def sha(p):return hashlib.file_digest(p.open('rb'),'sha256').hexdigest()
check((R/'complete.json').exists(),'capture incomplete')
state=json.loads(call(['docker','inspect',PROFILER]))[0]
check(not state['State']['Running'] and state['HostConfig']['Memory']==536870912,'profiler not idle')
classifier=(P/'classify.py').read_text().replace("binary = Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/histogram-bin/roll-engine')",'binary = Path(sys.argv[2])').replace("assert hashlib.sha256(binary.read_bytes()).hexdigest() == '5cfc79043a50d0ac228bdac3c66deb5fcf0dd34d73add389bf2773237e51ffd1'",'assert hashlib.sha256(binary.read_bytes()).hexdigest() == sys.argv[3]')
(R/'classify-binary.py').write_text(classifier)
try:
 call(['docker','update','--memory','2g','--memory-swap','2g',PROFILER]);call(['docker','start',PROFILER])
 for arm in ['baseline','candidate']:
  folder=R/arm;binary=folder/'roll-engine'
  for raw in sorted(folder.glob('*.data')):
   if raw.name.endswith('-replay.data'):continue
   replay=raw.with_name(raw.stem+'-replay.data');stack=raw.with_name(raw.stem+'-stacks.txt')
   subprocess.run(['python3','-I',str(P/'normalize-maps.py'),str(raw),str(binary),str(replay)],check=True)
   with stack.open('x') as log:subprocess.run(['docker','exec',PROFILER,'perf','script','-f','-i',str(replay),'-F','comm,pid,tid,time,cpu,event,ip,period','--no-inline'],stdout=log,check=True,timeout=180)
  subprocess.run(['python3','-I',str(R/'classify-binary.py'),str(folder),str(binary),sha(binary)],check=True)
finally:
 call(['docker','stop','-t','10',PROFILER]);call(['docker','update','--memory','512m','--memory-swap','512m',PROFILER])
 state=json.loads(call(['docker','inspect',PROFILER]))[0]
 check(not state['State']['Running'] and state['HostConfig']['Memory']==state['HostConfig']['MemorySwap']==536870912,'profiler cleanup')
 (R/'decode-cleanup.json').write_text(json.dumps({'profiler_stopped':True,'budget_restored_bytes':536870912},indent=2)+'\n')
print('Offline decode complete; no workloads run')
