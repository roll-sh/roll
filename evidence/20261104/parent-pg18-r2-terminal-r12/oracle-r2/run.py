from pathlib import Path
import datetime,hashlib,json,os,shlex,signal,subprocess,sys,time,traceback
P=Path(__file__).parent
NAME='t3-d4020b98-combined-pg18-oracles-r2'
IMAGE='sha256:898c60cb390d1085bc97bdd6a1ae968757421df394b071848b52b1c09c89d6a5'
started=time.monotonic(); owned=False; counter=0
records=[]
def now(): return datetime.datetime.now(datetime.timezone.utc).isoformat()
def run(tag,argv,stdin=None,check=True,timeout=60,final=False):
 global counter
 counter+=1;prefix=f'{counter:03d}-{tag}';out=P/(prefix+'.stdout');err=P/(prefix+'.stderr')
 rec={'tag':tag,'argv':argv,'shell_rendering':shlex.join(argv),'started_utc':now(),'stdin_path':str(stdin) if stdin else None}
 if stdin: rec['stdin_sha256']=hashlib.sha256(Path(stdin).read_bytes()).hexdigest()
 remaining=550-(time.monotonic()-started)
 if not final and remaining<1: raise TimeoutError('550 second internal job deadline')
 print('COMMAND '+rec['shell_rendering'],flush=True)
 try:
  with out.open('wb') as o,err.open('wb') as e:
   r=subprocess.run(argv,input=Path(stdin).read_bytes() if stdin else None,stdout=o,stderr=e,timeout=timeout if final else min(timeout,remaining))
  rec['actual_exit']=r.returncode
 except BaseException as exc:
  rec['exception']=repr(exc);rec['actual_exit']=None
  raise
 finally:
  rec.update(ended_utc=now(),stdout=str(out),stderr=str(err));records.append(rec)
  (P/(prefix+'.command.json')).write_text(json.dumps(rec,indent=2)+'\n')
  (P/'commands.json').write_text(json.dumps(records,indent=2)+'\n')
  for stream,path in [('STDOUT',out),('STDERR',err)]:
   if path.exists():
    print(prefix+' '+stream+' BEGIN',flush=True);sys.stdout.write(path.read_bytes().decode('utf8','replace'));sys.stdout.flush();print(prefix+' '+stream+' END',flush=True)
  print(prefix+' ACTUAL_EXIT '+str(rec.get('actual_exit')),flush=True)
 if check and r.returncode: raise RuntimeError(f'{tag} actual exit {r.returncode}')
 return out.read_text(errors='replace'),r.returncode

def psql(tag,db,sql):
 return run(tag,['docker','exec','-i','-e','PGCLIENTENCODING=UTF8',NAME,'psql','-h','127.0.0.1','-X','-a','-v','ON_ERROR_STOP=1','-U','postgres','-d',db,'-f','-'],P/(sql+'.sql'),timeout=90,check=False)
def stop(signum,frame): raise TimeoutError(f'received signal {signum}')
signal.signal(signal.SIGTERM,stop);signal.signal(signal.SIGINT,stop)
result={'evidence_job_id':'combined-pg18-oracles-r2','backing_thread_id':'thread:delegated-task:command%3Amcp%3A9e5a6feb-d435-4514-ac9d-07c1b03b444b%3Adelegate-task%3Apg-resume-20261104-combined-pg18-oracles-r1','started_utc':now(),'container_name':NAME,'image_id':IMAGE,'process_group':os.getpgrp(),'pid':os.getpid(),'internal_deadline_seconds':550,'outer_process_group_deadline_seconds':580}
exitcode=1
try:
 run('identity-health',['bash','-c','date -u --iso-8601=seconds; cat /proc/self/status; ps -o pid,ppid,pgid,ni,args -p $$ -p $PPID; df -B1 /home/meszmate/.cache; free -b; cat /proc/pressure/cpu; cat /tmp/pg-engine-heavy.lock.slots /tmp/pg-engine-heavy.lock.jobs; ls -ld /tmp/pg-engine-heavy.lock*'])
 free=os.statvfs(P);available=free.f_bavail*free.f_frsize
 result['free_bytes_before']=available
 if available<30*1024**3: raise RuntimeError('below 30GiB free disk floor')
 own,_=run('own-label-check',['docker','ps','-a','--filter','label=agent.worktree=t3-d4020b98','--no-trunc','--format','{{json .}}'])
 for line in own.splitlines():
  c=json.loads(line)
  if c.get('State')=='running': raise RuntimeError('own-label running container exists; no concurrent PG reference permitted')
 info,_=run('image-identity',['docker','image','inspect',IMAGE]);image=json.loads(info)[0]
 if 'PG_VERSION=18.6' not in image['Config']['Env']: raise RuntimeError('cached image is not PG18.6')
 # Empty tmpfs overrides image VOLUME: actual data goes only into ephemeral writable layer.
 argv=['docker','run','--pull=never','--detach','--rm','--name',NAME,'--label','agent.worktree=t3-d4020b98','--label','agent.evidence=combined-pg18-oracles-r2','--memory=512m','--memory-swap=512m','--cpus=2','--cpuset-cpus=4-7,22-25','--network=none','--tmpfs','/var/lib/postgresql:rw,size=1m','-e','PGDATA=/tmp/pg18-reference-data','-e','POSTGRES_HOST_AUTH_METHOD=trust','-e','POSTGRES_INITDB_ARGS=--locale=C --encoding=UTF8','--entrypoint','/bin/sh',IMAGE,'-c',"exec nice -n 19 docker-entrypoint.sh postgres -c shared_buffers=16MB -c max_connections=12 -c log_min_messages=debug1 -c log_error_verbosity=verbose -c log_line_prefix='%m [%p] ' -c listen_addresses=127.0.0.1 "]
 owned=True
 cid,_=run('container-start',argv);result['container_id']=cid.strip()
 inspection,_=run('container-inspect',['docker','inspect',NAME])
 inspected=json.loads(inspection)[0]
 if inspected.get('Mounts') or inspected['HostConfig'].get('PortBindings'): raise RuntimeError('Unexpected volume/bind mount or published port')
 ready=False
 for i in range(30):
  out,rc=run(f'readiness-tcp-{i:02d}',['docker','exec',NAME,'pg_isready','-h','127.0.0.1','-U','postgres','-d','postgres'],check=False,timeout=4)
  if rc==0:
   ver,vrc=run(f'verify-version-tcp-{i:02d}',['docker','exec',NAME,'psql','-h','127.0.0.1','-X','-At','-v','ON_ERROR_STOP=1','-U','postgres','-d','postgres','-c',"SELECT version(), current_setting('server_version_num')"],check=False,timeout=5)
   if vrc==0:
    result['verified_version']=ver.strip()
    if 'PostgreSQL 18.6 ' not in ver or '|180006' not in ver: raise RuntimeError('SELECT version() is not PG18.6/180006; no fallback')
    ready=True;break
  time.sleep(1)
 if not ready: raise RuntimeError('Final TCP postmaster/version not ready after bounded 30 attempts')
 client,_=run('verify-psql18',['docker','exec',NAME,'psql','--version'])
 result['verified_psql_version']=client.strip()
 if 'psql (PostgreSQL) 18.6' not in client: raise RuntimeError('psql client is not 18.6')
 run('postgres-cpu-identity',['docker','exec',NAME,'sh','-c','cat /proc/1/status; cat /proc/1/stat; cat /sys/fs/cgroup/cpu.max; cat /sys/fs/cgroup/cpuset.cpus.effective; cat /sys/fs/cgroup/memory.max'])
 _,admin_rc=psql('admin','postgres','admin')
 if admin_rc: raise RuntimeError(f'admin actual exit {admin_rc}')
 _,setup_rc=psql('planner-db-setup','postgres','planner-db-setup')
 if setup_rc: raise RuntimeError(f'planner DB setup actual exit {setup_rc}')
 for c in json.loads((P/'cases.json').read_text()):
  print('CASE_BEGIN '+json.dumps(c),flush=True)
  _,rc=psql(c['name'],c['db'],c['name'])
  result.setdefault('fixture_exits',{})[c['name']]=rc
  print('CASE_END '+c['name'],flush=True)
 for name,db in [('171-parse-bind-generic','planner_generic'),('171-parse-bind-parallel','planner_parallel')]:
  _,src=psql(name+'-settings',db,'planner-settings')
  if src: raise RuntimeError(f'planner settings actual exit {src}')
  print('PLANNER_CONTROL_BEGIN '+name,flush=True)
  _,rc=psql(name,db,name)
  result.setdefault('planner_exits',{})[name]=rc
  print('PLANNER_CONTROL_END '+name,flush=True)
 run('postgres-resource-after',['docker','exec',NAME,'sh','-c','cat /proc/1/status; cat /proc/1/stat; cat /sys/fs/cgroup/cpu.max; cat /sys/fs/cgroup/cpuset.cpus.effective; cat /sys/fs/cgroup/memory.max; cat /sys/fs/cgroup/memory.events'])
 result['capture_completed']=True
 exitcode=0 if all(v==0 for v in result.get('fixture_exits',{}).values()) and all(v==0 for v in result.get('planner_exits',{}).values()) else 1
except BaseException as exc:
 result['exception']=repr(exc);traceback.print_exc()
finally:
 signal.signal(signal.SIGTERM,signal.SIG_IGN);signal.signal(signal.SIGINT,signal.SIG_IGN)
 if owned:
  try:
   run('container-logs',['docker','logs','--timestamps',NAME],check=False,timeout=15,final=True)
   run('container-final-inspect',['docker','inspect',NAME],check=False,timeout=10,final=True)
  except BaseException as exc: result['log_capture_exception']=repr(exc)
  try:
   # Remove only the explicitly named container carrying both labels from this job.
   ident,rc=run('cleanup-identity',['docker','inspect','--format','{{index .Config.Labels "agent.worktree"}}|{{index .Config.Labels "agent.evidence"}}',NAME],check=False,timeout=10,final=True)
   if rc==0 and ident.strip()=='t3-d4020b98|combined-pg18-oracles-r2':
    _,rc=run('container-remove',['docker','rm','-f',NAME],check=False,timeout=15,final=True)
    result['container_remove_actual_exit']=rc
   elif rc: result['container_already_absent']=True
   else: result['cleanup_refused_label_mismatch']=ident;exitcode=1
   absent,rc=run('container-absence',['docker','ps','-a','--filter','name=^/'+NAME+'$','--format','{{.ID}} {{.Names}}'],check=False,timeout=10,final=True)
   result['container_absence_confirmed']=rc==0 and not absent.strip()
   if not result['container_absence_confirmed']:exitcode=1
  except BaseException as exc:result['cleanup_exception']=repr(exc);exitcode=1
 try:run('health-after',['bash','-c','date -u --iso-8601=seconds; df -B1 /home/meszmate/.cache; cat /proc/pressure/cpu; cat /tmp/pg-engine-heavy.lock.slots /tmp/pg-engine-heavy.lock.jobs'],check=False,timeout=10,final=True)
 except BaseException as exc:result['health_after_exception']=repr(exc)
 result.update(ended_utc=now(),elapsed_seconds=time.monotonic()-started,runner_actual_exit=exitcode)
 (P/'run-result.json').write_text(json.dumps(result,indent=2)+'\n');print('RUN_RESULT '+json.dumps(result),flush=True)
sys.exit(exitcode)
