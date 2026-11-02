"""Comparative CPU ancestry only, not another candidate acceptance attempt."""
from pathlib import Path
import datetime,hashlib,importlib.util,json,os,shutil,signal,socket,subprocess,sys,time
sys.dont_write_bytecode=True
REPO=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
SOURCE=Path('/home/meszmate/.cache/pg-engine-perf-20261102/gather-once')
ROOT=Path('/home/meszmate/.cache/pg-engine-perf-20261101/gather-fallback-20261102')
ROOT.mkdir(exist_ok=False)
DIAG=Path('/home/meszmate/.cache/pg-engine-perf-20261102/planning-diagnostic/diagnose.py')
spec=importlib.util.spec_from_file_location('diag',DIAG);d=importlib.util.module_from_spec(spec);spec.loader.exec_module(d)
wire=d.load_module('wire',d.WIRE);paired=d.load_module('paired',d.RUNNER)
PROFILER='813b04eed73f'
NORMALIZE=Path('/home/meszmate/.cache/pg-engine-perf-20261101/planning-cpu/normalize-maps.py')
engines=[];profiler_changed=False;before=None

def check(b,s):
 if not b:raise RuntimeError(s)
def save(p,x):
 with p.open('x') as f:json.dump(x,f,indent=2);f.write('\n')
def call(args,timeout=120):return subprocess.check_output([str(x) for x in args],text=True,timeout=timeout).strip()
def interrupted(signum,frame):raise KeyboardInterrupt('signal '+str(signum))
signal.signal(signal.SIGTERM,interrupted)
try:
 before=json.loads(call(['docker','inspect',PROFILER]))[0]
 check(not before['State']['Running'],'profiler already in use')
 check(before['Config']['Labels'].get('agent.worktree')=='t3-d4020b98','unowned profiler')
 check(before['HostConfig']['Memory']==536870912 and before['HostConfig']['MemorySwap']==536870912,'unexpected profiler budget')
 save(ROOT/'profiler-before.json',before)
 profiler_changed=True
 call(['docker','update','--memory','2g','--memory-swap','2g',PROFILER]);call(['docker','start',PROFILER])
 call(['docker','exec',PROFILER,'perf','--version'])
 d.require_stopped(d.FIXTURE);fixture=d.tree_manifest(d.FIXTURE);save(ROOT/'fixture-before.json',fixture)
 inputs=[Path(__file__),DIAG,NORMALIZE,d.RUNNER,d.WIRE,d.CLIENT]
 for arm,binary in [('baseline',d.ENGINE),('candidate',SOURCE/'candidate-bin/roll-engine')]:
  folder=ROOT/arm;folder.mkdir();shutil.copy2(binary,folder/'roll-engine');inputs.append(folder/'roll-engine')
 save(ROOT/'frame.json',{'at_utc':d.stamp(),'purpose':'Separate comparative CPU ancestry for zero-setup declined proof. No acceptance or profiled speed claim. Three counterbalanced captures per workload/arm;49Hz20s window inside30s checked invocation.','inputs':{str(p):d.sha(p) for p in inputs}})
 targets=[]
 for index,arm in enumerate(['baseline','candidate']):
  folder=ROOT/arm;binary=folder/'roll-engine';data=folder/'data';port=53660+index
  call(['cp','-a','--reflink=auto',d.FIXTURE,data]);check(d.tree_manifest(data)==fixture,'fixture clone')
  with socket.socket() as s:s.bind(('127.0.0.1',port))
  argv=['taskset','-c','28-31',str(binary),'--data',str(data),'--listen',f'127.0.0.1:{port}','--password','pw','--cache-mb','2048','-c','maintenance_work_mem=256MB','-c','max_connections=100','-c','parallel_setup_cost=0','-c','parallel_tuple_cost=0','-c','min_parallel_table_scan_size=0','-c','min_parallel_index_scan_size=0']
  with (folder/'server.log').open('x') as log:e=subprocess.Popen(argv,stdout=log,stderr=subprocess.STDOUT)
  engines.append((arm,e,data));save(folder/'owned.json',{'pid':e.pid,'argv':argv})
  for attempt in range(120):
   check(e.poll() is None,'engine died')
   try:
    with socket.create_connection(('127.0.0.1',port),timeout=.5):break
   except OSError:time.sleep(.25)
  else:raise RuntimeError('engine not ready')
  check(d.sha(Path('/proc',str(e.pid),'exe'))==d.sha(binary),'runtime binary identity')
  save(folder/'preflight.json',d.preflight(wire,port,'auto'))
  targets.append({'arm':arm,'pid':e.pid,'port':port,'folder':folder,'binary':binary})
 check(json.loads((ROOT/'baseline/preflight.json').read_text())['cases']==json.loads((ROOT/'candidate/preflight.json').read_text())['cases'],'preflight mismatch')
 for round_no in range(3):
  for case,clients in [('range_scan_100',8),('point_select',1)]:
   for t in (targets if round_no%2==0 else list(reversed(targets))):
    label=f'{round_no:02}-{case}-{clients}';folder=t['folder'];raw=folder/(label+'.data')
    command=['taskset','-c','32-35',str(d.CLIENT),'--target','wire','--label',label,'--port',str(t['port']),'--db',d.DB,'--mem-pid',str(t['pid']),'oltp','--rows','200000','--only',case,'--clients',str(clients),'--secs','30']
    started=time.monotonic();cpu_before=paired.cpu({'pid':t['pid']},100)
    with (folder/(label+'-client.log')).open('x') as log:
     client=subprocess.Popen(command,stdout=log,stderr=subprocess.STDOUT)
     try:
      time.sleep(2)
      perf=['docker','exec',PROFILER,'perf','record','-e','cpu-clock','-F','49','-T','--sample-cpu','--call-graph','dwarf,65528','-p',str(t['pid']),'-o',str(raw),'--','sleep','20']
      with (folder/(label+'-record.log')).open('x') as plog:subprocess.run(perf,stdout=plog,stderr=subprocess.STDOUT,timeout=60,check=True)
      call(['docker','exec',PROFILER,'chmod','644',raw])
      client.wait(timeout=40)
     finally:
      if client.poll() is None:client.terminate();client.wait(timeout=10)
    check(client.returncode==0,'client failed')
    result=paired.parse((folder/(label+'-client.log')).read_text(),label,case,clients)
    check(int(result['errors'])==0 and int(result['ops'])==int(result['checked_ops']),'work errors')
    save(folder/(label+'-invocation.json'),{'command':command,'exit':client.returncode,'result':result,'elapsed_parent_s':time.monotonic()-started,'server_cpu_s':paired.cpu({'pid':t['pid']},100)-cpu_before,'profiled':True})
    print('Captured',t['arm'],label,flush=True)
 check(d.tree_manifest(d.FIXTURE)==fixture,'source fixture changed')
 save(ROOT/'complete.json',{'at_utc':d.stamp(),'diagnostic_only':True})
finally:
 signal.signal(signal.SIGTERM,signal.SIG_IGN);signal.signal(signal.SIGINT,signal.SIG_IGN)
 cleanup=[]
 for arm,e,data in engines:
  try:d.stop(e);cleanup.append({'arm':arm,'pid':e.pid,'exit':e.returncode,'stopped':True})
  except Exception as error:cleanup.append({'arm':arm,'pid':e.pid,'stopped':e.poll() is not None,'error':repr(error)})
 if profiler_changed:
  try:call(['docker','stop','-t','10',PROFILER])
  finally:call(['docker','update','--memory','512m','--memory-swap','512m',PROFILER])
 save(ROOT/'cleanup.json',cleanup)
 check(all(x.get('stopped') and x.get('exit')==0 for x in cleanup),'cleanup failed')
 if before is not None:
  after=json.loads(call(['docker','inspect',PROFILER]))[0];save(ROOT/'profiler-after.json',after)
  check(not after['State']['Running'] and after['HostConfig']['Memory']==after['HostConfig']['MemorySwap']==536870912,'profiler not restored')
 for arm,e,data in engines:
  check(not Path('/proc',str(e.pid)).exists() and data.parent==ROOT/arm and not data.is_symlink(),'unsafe cleanup')
  shutil.rmtree(data)
 print('Cleaned up owned engines/data and stopped/restored profiler budget',flush=True)
