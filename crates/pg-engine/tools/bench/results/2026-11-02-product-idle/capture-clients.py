#!/usr/bin/env python3
import argparse,hashlib,importlib.util,json,os,socket,subprocess,sys,time
from pathlib import Path
from datetime import datetime,timezone
sys.dont_write_bytecode=True
REPO=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
RUNNER=REPO/'crates/pg-engine/tools/bench/idle.py'
spec=importlib.util.spec_from_file_location('idle',RUNNER);idle=importlib.util.module_from_spec(spec);spec.loader.exec_module(idle)
socket.setdefaulttimeout(10)
parser=argparse.ArgumentParser()
parser.add_argument('--mode',choices=['idle','prisma'],default='idle')
parser.add_argument('--out',type=Path,required=True)
parser.add_argument('--engine',action='append',required=True,help='name=/absolute/binary')
parser.add_argument('--rounds',type=int,default=1)
parser.add_argument('--seconds',type=float,default=5)
parser.add_argument('--phases',nargs='+',default=['disconnected','connected','released'])
parser.add_argument('--port',type=int,default=53360)
a=parser.parse_args();a.out.mkdir(parents=True,exist_ok=False)
commands=[];owned=[];container=None

def save(name,value): (a.out/name).write_text(json.dumps(value,indent=2)+'\n')
def call(args):
 commands.append({'at':datetime.now(timezone.utc).isoformat(),'argv':list(map(str,args))});save('commands.json',commands)
 return subprocess.check_output(list(map(str,args)),text=True).strip()
def free(port):
 with socket.socket() as s:s.bind(('127.0.0.1',port))
def wait(port):
 for _ in range(120):
  try:
   with socket.create_connection(('127.0.0.1',port),timeout=0.5):return
  except OSError:time.sleep(0.5)
 raise RuntimeError('server not ready on '+str(port))
def cgroup(pid):return '/sys/fs/cgroup'+Path(f'/proc/{pid}/cgroup').read_text().strip().split('::',1)[1]
def identity(pid):
 exe=Path(f'/proc/{pid}/exe')
 return {'pid':pid,'exe':str(exe.resolve()),'sha256':hashlib.sha256(exe.read_bytes()).hexdigest(),'cmdline':Path(f'/proc/{pid}/cmdline').read_bytes().replace(b'\0',b' ').decode(),'affinity':sorted(os.sched_getaffinity(pid)),'cgroup':cgroup(pid)}

try:
 stamp=datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')
 targets=[];runtime={}
 for index,entry in enumerate(a.engine):
  name,binary=entry.split('=',1);binary=Path(binary).resolve();port=a.port+index;free(port)
  unit=f't3-d4020b98-idle-{name}-{stamp}';data=a.out/(name+'-data');data.mkdir()
  args=['systemd-run','--user','--wait','--pipe','--quiet','--collect','--unit',unit,'-p','MemoryMax=512M','-p','MemorySwapMax=0','-p','AllowedCPUs=28-31','-p','CPUQuota=400%','-p','TimeoutStopSec=30','taskset','-c','28-31',str(binary),'--data',str(data),'--listen',f'127.0.0.1:{port}','--password','pw','--cache-mb','64','--plan-cache-mb','36','--idle-close-secs','5']
  commands.append({'argv':args});save('commands.json',commands)
  log=(a.out/(name+'-server.log')).open('w')
  process=subprocess.Popen(args,stdout=log,stderr=subprocess.STDOUT)
  owned.append({'unit':unit,'process':process,'log':log})
  wait(port)
  pid=int(call(['systemctl','--user','show',unit,'--property=MainPID','--value']))
  runtime[name]=identity(pid);owned[-1]['pid']=pid
  targets.append({'name':name,'port':port,'cgroup':cgroup(pid),'kind':'engine','leader_pid':pid,'leader_start_ticks':idle.process_start(pid),'sha256':runtime[name]['sha256']})
 pgport=a.port+len(a.engine);free(pgport)
 image='sha256:898c60cb390d1085bc97bdd6a1ae968757421df394b071848b52b1c09c89d6a5'
 name='t3-d4020b98-idle-pg-'+stamp
 data=a.out/'pg-data';data.mkdir()
 container=call(['docker','run','-d','--name',name,'--label','agent.worktree=t3-d4020b98','--label','agent.task=product-idle','--memory=512m','--memory-swap=512m','--cpuset-cpus=28-31','--cpus=4','-p',f'127.0.0.1:{pgport}:5432','-e','POSTGRES_PASSWORD=pw','-e','POSTGRES_INITDB_ARGS=--locale=C --encoding=UTF8','--mount',f'type=bind,src={data},dst=/var/lib/postgresql',image,'postgres','-c','shared_buffers=64MB','-c','max_connections=100','-c','jit=off','-c','fsync=on','-c','synchronous_commit=on','-c','full_page_writes=on'])
 save('pg-container.json',json.loads(call(['docker','inspect',container])))
 wait(pgport)
 pid=int(call(['docker','inspect',container,'--format','{{.State.Pid}}']))
 runtime['postgres']={'pid':pid,'cgroup':cgroup(pid),'image':image,'affinity':sorted(os.sched_getaffinity(pid))}
 targets.append({'name':'postgres','port':pgport,'cgroup':cgroup(pid),'kind':'postgres','leader_pid':pid,'leader_start_ticks':idle.process_start(pid),'image':image})
 runtime['postgres']['binary_sha256']=call(['docker','exec',container,'sha256sum','/usr/local/bin/postgres']).split()[0]
 save('runtime.json',runtime)
 db='bench_idle_'+stamp.lower()
 for target in targets:
  for attempt in range(120):
   try:
    conn=idle.connect(target,'postgres')
    assert idle.query(conn,'SELECT 1')==([23],[[b'1']])
    break
   except (OSError,EOFError):
    if attempt==119:raise
    time.sleep(0.5)
  try:idle.query(conn,'CREATE DATABASE '+db)
  finally:conn.s.close()
 config={'out':str(a.out/'observations'),'db':db,'rounds':a.rounds,'seconds':a.seconds,'sessions':64,'settle_seconds':5,'release_seconds':15,'phases':a.phases,'targets':targets,'diagnostic_only':a.rounds<5,'server_cpus':[28,29,30,31]}
 save('config.json',config)
 save('data-filesystem.json',json.loads(call(['findmnt','-T',str(a.out),'--json','-o','SOURCE,FSTYPE,TARGET'])))
 if a.mode=='prisma':
  rc=subprocess.call([sys.executable,str(a.out.parent/'run-clients.py'),str(a.out/'config.json'),str(a.out/'prisma')],stdout=(a.out/'prisma.log').open('w'),stderr=subprocess.STDOUT)
  if rc:raise RuntimeError('Prisma check failed: '+str(rc))
 else:
  rc=subprocess.call(['taskset','-c','32-35',sys.executable,str(RUNNER),str(a.out/'config.json')],stdout=(a.out/'probe.log').open('w'),stderr=subprocess.STDOUT)
  if rc:raise RuntimeError('probe failed: '+str(rc))
  rc=subprocess.call([sys.executable,str(a.out.parent/'protocol-idle.py'),str(a.out/'config.json'),str(a.out/'protocol-results.json')],stdout=(a.out/'protocol.log').open('w'),stderr=subprocess.STDOUT)
  if rc:raise RuntimeError('protocol idle check failed: '+str(rc))
 save('complete.json',{'exit':rc,'at':datetime.now(timezone.utc).isoformat()})
finally:
 cleanup=[]
 for item in owned:
  result=subprocess.run(['systemctl','--user','stop',item['unit']],capture_output=True,text=True)
  try:code=item['process'].wait(timeout=35)
  except subprocess.TimeoutExpired:code=None
  item['log'].close()
  cleanup.append({'unit':item['unit'],'pid':item.get('pid'),'stop_exit':result.returncode,'wrapper_exit':code,'pid_absent':not Path('/proc/'+str(item.get('pid'))).exists()})
 if container:
  logs=subprocess.run(['docker','logs',container],capture_output=True,text=True)
  (a.out/'postgres-server.log').write_text(logs.stdout+logs.stderr)
  stop=subprocess.run(['docker','stop','-t','30',container],capture_output=True,text=True)
  info=subprocess.run(['docker','inspect',container,'--format','{{json .State}}'],capture_output=True,text=True)
  remove=subprocess.run(['docker','rm',container],capture_output=True,text=True)
  cleanup.append({'container':container,'stop_exit':stop.returncode,'state':json.loads(info.stdout) if info.returncode==0 else info.stderr,'remove_exit':remove.returncode})
 save('cleanup.json',cleanup)
 print(json.dumps(cleanup),flush=True)
