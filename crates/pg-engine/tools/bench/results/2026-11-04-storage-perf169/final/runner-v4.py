import pathlib as P, subprocess as S, json, time, os, sys, hashlib, threading, socket, traceback, statistics, signal
from build import R, SIDES, stable, sha
PG='postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873'
IMG='debian@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad'
LABEL='agent-a76932e8127f6d35b'; PORT=53390; OWN=[]; SEQ=max([int(x.name.split('-')[0]) for x in R.glob('[0-9][0-9][0-9][0-9]-*.log')] or [0]); CLIENT=None; ENGINE=None; DATA=None; SAMPLING=False; LAST_SNAP={}
LOG=R/'commands.jsonl'; RESULTS=R/'results.jsonl'
def emit(path,obj):
    with path.open('a') as f:f.write(json.dumps(obj,sort_keys=True)+'\n');f.flush()
def run(cmd,tag,timeout=600,input=None,check=True):
    global SEQ
    SEQ+=1; path=R/f'{SEQ:04d}-{tag}.log'; t=time.monotonic();rc=None
    with path.open('xb') as f:
      try:
        q=S.run(cmd,input=input,stdout=f,stderr=S.STDOUT,timeout=timeout);rc=q.returncode
      except S.TimeoutExpired:rc=124
      finally:emit(LOG,dict(tag=tag,cmd=cmd,exit=rc,wall=time.monotonic()-t,log=path.name,at=time.time()))
    if check and rc!=0:raise RuntimeError(f'{tag} exit={rc}: {path}')
    return path.read_text(errors='replace'),rc

def refuse(name,data=None,probe=None):
    if data is not None and P.Path(data).exists():raise RuntimeError(f'REFUSE existing data {data}')
    q=probe(name) if probe else S.run(['docker','container','inspect',name],stdout=S.DEVNULL,stderr=S.DEVNULL).returncode
    if q==0:raise RuntimeError(f'REFUSE existing container {name}')
    if q!=1:raise RuntimeError(f'cannot establish container absence {name}: {q}')
def create(name,cpus,mem,image,args,extra=[]):
    refuse(name)
    cmd=['docker','create','--name',name,'--label','agent.worktree='+LABEL,'--label','agent.lane=storage_perf169','--network','host','--cpuset-cpus',cpus,'--cpu-shares','2','--memory',mem,'--memory-swap',mem,'--shm-size','1g',*extra,image,*args]
    out,_=run(cmd,'create-'+name);cid=out.strip();assert len(cid)==64
    OWN.append(cid);emit(R/'resources.jsonl',dict(id=cid,name=name,created=time.time(),command=cmd))
    run(['docker','start',cid],'start-'+name)
    return cid

def inspect(cid):
    out,_=run(['docker','inspect',cid],'inspect');return json.loads(out)[0]
def cg(cid):return P.Path('/sys/fs/cgroup/system.slice/docker-'+cid+'.scope')
def kv(path):
    try:return {x.split()[0]:int(x.split()[1]) for x in path.read_text().splitlines() if len(x.split())==2}
    except FileNotFoundError:return {}
def raw_snap(cid):
    c=cg(cid)
    if not (c/'cpu.stat').exists():
      return dict(LAST_SNAP.get(cid,dict(cpu={},memory={},anon_shmem=0,current=0,peak=0,io={},pressure={})), final_counter_sample_incomplete=True)
    m=kv(c/'memory.stat');io={};io_devices={}
    data_dev=os.stat(DATA or R).st_dev;selected_device=f'{os.major(data_dev)}:{os.minor(data_dev)}'
    try:
      for line in (c/'io.stat').read_text().splitlines():
        parts=line.split();values={k:int(v) for k,v in (x.split('=') for x in parts[1:])};io_devices[parts[0]]=values
      io=io_devices.get(selected_device,{})
    except FileNotFoundError:pass
    result=dict(cpu=kv(c/'cpu.stat'),memory=m,anon_shmem=m.get('anon',0)+m.get('shmem',0),current=int((c/'memory.current').read_text()) if (c/'memory.current').exists() else 0,peak=int((c/'memory.peak').read_text()) if (c/'memory.peak').exists() else 0,io=io,io_devices=io_devices,io_selected_device=selected_device,pressure={k:(c/(k+'.pressure')).read_text() for k in ['cpu','io','memory'] if (c/(k+'.pressure')).exists()})
    LAST_SNAP[cid]=result;return result
def snap(cid):
    try:return raw_snap(cid)
    except FileNotFoundError:
      assert cid in LAST_SNAP
      return dict(LAST_SNAP[cid], final_counter_sample_incomplete=True)
def disk():
    total=allocated=0;files=0
    if DATA:
      for root,dirs,names in os.walk(DATA,followlinks=True):
        for n in names:
          try:z=(P.Path(root)/n).stat();total+=z.st_size;allocated+=z.st_blocks*512;files+=1
          except FileNotFoundError:pass
    return dict(apparent=total,allocated=allocated,files=files)
def host():return dict(load=P.Path('/proc/loadavg').read_text(),pressure={k:P.Path('/proc/pressure/'+k).read_text() for k in ['cpu','io','memory']},stat=P.Path('/proc/stat').read_text())
def machine(tag):
    run(['bash','-c','ps -eo pid,psr,ni,pcpu,pmem,comm,args --sort=-pcpu | head -25'],'competing-'+tag)
    run(['docker','ps','--no-trunc','--format','{{.ID}} {{.Names}} {{.Status}}'],'containers-'+tag)

def sql(text,tag='sql',check=True):
    assert CLIENT and ENGINE
    return run(['docker','exec','-e','PGPASSWORD=pw',CLIENT,'psql','-X','-w','-h','127.0.0.1','-p',str(PORT),'-U','postgres','-d','bench','-v','ON_ERROR_STOP=1','-At','-c',text],tag,check=check)
def psqlcmd(text):return ['docker','exec','-e','PGPASSWORD=pw',CLIENT,'psql','-X','-w','-h','127.0.0.1','-p',str(PORT),'-U','postgres','-d','bench','-v','ON_ERROR_STOP=1','-At','-c',text]
def loadcmd(table='data'):return psqlcmd("\\copy "+table+" FROM '/input/rows.csv' WITH (FORMAT csv)")
def initcmd():return ['docker','exec','-e','PGPASSWORD=pw',CLIENT,'pgbench','-h','127.0.0.1','-p',str(PORT),'-U','postgres','-i','-s','10','bench']
def delta(a,b,section):return {k:b[section].get(k,0)-a[section].get(k,0) for k in set(a[section])|set(b[section])}
def observed(cmd,tag,profile=False,timeout=600):
    stable();machine(tag);a=snap(ENGINE);ca=snap(CLIENT);h=host();samples=[];errors=[];done=threading.Event()
    def sample():
      lastdisk=0;d={}
      try:
        while not done.is_set():
          now=time.monotonic()
          if now-lastdisk>=.2:d=disk();lastdisk=now
          samples.append(dict(at=time.time(),server=snap(ENGINE),client=snap(CLIENT),disk=d,host=host()))
          done.wait(.1)
      except BaseException:errors.append(traceback.format_exc());done.set()
    th=threading.Thread(target=sample);th.start();t=time.monotonic();rc=None
    try:
      actual=cmd
      if profile:
        pid=inspect(ENGINE)['State']['Pid']
        actual=['sudo','-n','perf','record','-F','49','-g','--call-graph','dwarf,8192','-p',str(pid),'-o',str(R/(tag+'.perf.data')),'--',*cmd]
      out,rc=run(actual,tag,timeout=timeout,check=False)
    finally:
      wall=time.monotonic()-t;b=snap(ENGINE);cb=snap(CLIENT);done.set();th.join()
      (R/(tag+'.samples.json')).write_text(json.dumps(samples));d=disk()
      x=dict(tag=tag,profile=profile,exit=rc,measurement_errors=errors,wall=wall,server_before=a,server_after=b,client_before=ca,client_after=cb,server_cpu=delta(a,b,'cpu'),client_cpu=delta(ca,cb,'cpu'),server_io=delta(a,b,'io'),client_io=delta(ca,cb,'io'),sampled_peak_anon_shmem=max([z['server']['anon_shmem'] for z in samples]or[0]),sampled_disk_highwater=max([z['disk'].get('allocated',0) for z in samples]or[0]),disk_after=d,host_before=h,host_after=host())
      emit(RESULTS,x);stable()
    assert not errors,errors
    assert rc==0,f'{tag}: nonzero exit {rc}'
    if profile:
      run(['sudo','-n','chown',str(os.getuid())+':'+str(os.getgid()),str(R/(tag+'.perf.data'))],'profile-owner')
      run(['perf','report','--stdio','--no-children','--percent-limit','1','-i',str(R/(tag+'.perf.data'))],tag+'-profile-self')
      run(['perf','report','--stdio','--children','--percent-limit','2','-i',str(R/(tag+'.perf.data'))],tag+'-profile-children')
    return out,x

def window(tag,settle=10,duration=10):
    time.sleep(settle);a=snap(ENGINE);c=snap(CLIENT);h=host();mem=[];t=time.monotonic()
    while time.monotonic()-t<duration:
      mem.append(dict(at=time.time(),server=snap(ENGINE),client=snap(CLIENT),disk=disk()));time.sleep(.2)
    b=snap(ENGINE);cb=snap(CLIENT)
    emit(R/'windows.jsonl',dict(tag=tag,settle=settle,duration=time.monotonic()-t,server_before=a,server_after=b,client_before=c,client_after=cb,server_cpu=delta(a,b,'cpu'),client_cpu=delta(c,cb,'cpu'),anon_shmem_median=statistics.median(z['server']['anon_shmem'] for z in mem),host_before=h,host_after=host(),samples=mem))

def audit_affinity(cid,cpus,mem):
    z=inspect(cid);pid=z['State']['Pid'];c=cg(cid)
    proof=dict(inspect=z,proc_status=P.Path(f'/proc/{pid}/status').read_text(),proc_cgroup=P.Path(f'/proc/{pid}/cgroup').read_text(),controls={x:(c/x).read_text() for x in ['cpu.max','cpu.weight','cpuset.cpus.effective','memory.max','memory.swap.max']})
    (R/(cid[:12]+'.controls.json')).write_text(json.dumps(proof,indent=2))
    assert z['HostConfig']['CpusetCpus']==cpus and z['HostConfig']['CpuShares']==2
    assert proof['controls']['cpu.max'].startswith('max ') and proof['controls']['cpu.weight'].strip()=='1'
    assert proof['controls']['cpuset.cpus.effective'].strip()==cpus
    assert int(proof['controls']['memory.max'])==mem and proof['controls']['memory.swap.max'].strip()=='0'
    for task in P.Path(f'/proc/{pid}/task').iterdir():
      line=next(x for x in (task/'status').read_text().splitlines() if x.startswith('Cpus_allowed_list:'))
      assert line.split()[1]==cpus,line

def free_port():
    with socket.socket() as z:
      z.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
      z.bind(('127.0.0.1',PORT))

def start(side,tag,data=None):
    global ENGINE,DATA
    stable();assert ENGINE is None
    name='a169-resume-perf-'+tag;data=data or R/'data'/tag
    refuse(name,data)
    free_port()
    P.Path(data).mkdir(parents=True);DATA=P.Path(data).resolve()
    binary=next(x['binary'] for x in json.loads((R/'builds.json').read_text()) if x['side']==side)
    ENGINE=create(name,'14-17','4g',IMG,[binary,'--data',str(DATA),'--listen',f'127.0.0.1:{PORT}','--password','pw','--cache-mb','2048','-c','max_connections=300'],['--user',f'{os.getuid()}:{os.getgid()}','-v',binary+':'+binary+':ro','-v',str(DATA)+':'+str(DATA),'-v','/usr/share/zoneinfo:/usr/share/zoneinfo:ro','-e','TMPDIR='+str(DATA)])
    audit_affinity(ENGINE,'14-17',4*1024**3)
    # TCP readiness is bounded and never runs SQL setup against an unowned listener.
    for i in range(300):
      try:
        with socket.create_connection(('127.0.0.1',PORT),timeout=.2):break
      except OSError:time.sleep(.2)
    else:raise RuntimeError('readiness timeout')
    out,_=run(['docker','exec','-e','PGPASSWORD=pw',CLIENT,'psql','-X','-w','-h','127.0.0.1','-p',str(PORT),'-U','postgres','-d','postgres','-v','ON_ERROR_STOP=1','-Atc','CREATE DATABASE bench'],'create-db')
    sql('SELECT version(); SHOW synchronous_commit; SHOW work_mem; SHOW maintenance_work_mem; SHOW max_connections;','settings')

def stop(tag):
    global ENGINE
    if ENGINE:
      cid=ENGINE
      out,x=observed(['docker','stop','--time','120',cid],tag+'-close',timeout=150)
      z=inspect(cid);assert z['State']['ExitCode']==0,z['State']
      run(['docker','logs',cid],tag+'-engine')
      run(['docker','rm',cid],tag+'-remove');OWN.remove(cid);ENGINE=None
      return x

def check_data(table='data'):
    exp=json.loads((R/'input.json').read_text())
    query=f'SELECT count(*),sum(id::bigint),sum(bucket::bigint),sum(length(payload)::bigint),sum(ascii(substr(payload,1,1))::bigint) FROM {table}'
    out,_=sql(query,'validate-'+table);want='|'.join(str(exp[k]) for k in ['rows','sum_id','sum_bucket','sum_length','sum_ascii'])
    assert out.strip()==want,(out,want)
    emit(R/'correctness.jsonl',dict(table=table,expected=want,actual=out.strip(),failures=0,engine=ENGINE))
def check_pgbench():
    out,_=sql('SELECT count(*),sum(aid::bigint),sum(bid::bigint),sum(abalance::bigint),sum(length(trim(filler))::bigint) FROM pgbench_accounts; SELECT count(*),sum(bid::bigint),sum(bbalance::bigint) FROM pgbench_branches; SELECT count(*),sum(tid::bigint),sum(bid::bigint),sum(tbalance::bigint) FROM pgbench_tellers; SELECT count(*) FROM pgbench_history;','validate-pgbench')
    want='1000000|500000500000|5500000|0|0\n10|55|0\n100|5050|550|0\n0'
    assert out.strip()==want,(out,want)
    emit(R/'correctness.jsonl',dict(table='pgbench',expected=want,actual=out.strip(),failures=0,engine=ENGINE))
def check_index():
    out,_=sql("SELECT count(*) FROM pg_indexes WHERE tablename='data' AND indexname='data_bucket_idx'; SET enable_seqscan=off; EXPLAIN SELECT id FROM data WHERE bucket=417; SELECT count(*),sum(id::bigint) FROM data WHERE bucket=417;",'validate-index')
    assert out.splitlines()[0]=='1' and out.strip().endswith('1000|499917000') and 'Index' in out,out
    emit(R/'correctness.jsonl',dict(table='index',expected='1 index, indexed plan, 1000|499917000',actual=out,failures=0,engine=ENGINE))
def setup_data():sql('CREATE TABLE data(id integer NOT NULL,bucket integer NOT NULL,payload text NOT NULL)','create-data')

def trial(side,work,k,profile=False):
    tag=f'{work}-{k}-{side}'+('-profile' if profile else '')
    start(side,tag)
    try:
      if work=='pgbench':
        out,x=observed(initcmd(),tag,profile);assert 'done in ' in out,out;check_pgbench()
      else:
        setup_data();out,_=sql('SELECT count(*) FROM data','empty');assert out.strip()=='0'
        if work=='copy':
          out,x=observed(loadcmd(),tag,profile);assert 'COPY 1000000' in out,out;check_data()
        elif work=='index':
          out,_=run(loadcmd(),'untimed-index-load');assert 'COPY 1000000' in out;check_data();window(tag+'-setup',5,5)
          out,x=observed(psqlcmd('CREATE INDEX data_bucket_idx ON data(bucket,id)'),tag,profile);assert 'CREATE INDEX' in out;check_data();check_index()
      window(tag+'-loaded',5,5)
      stop(tag)
    except BaseException:
      raise

def input_data():
    d=R/'input';d.mkdir();total=0
    with (d/'rows.csv').open('x') as f:
      for i in range(1,1000001):
        payload=hashlib.md5(str(i).encode()).hexdigest()*8;total+=ord(payload[0]);f.write(f'{i},{i%1000},{payload}\n')
    (d/'idle.sql').write_text('SELECT 1;\n\\sleep 60 s\n')
    (R/'input.json').write_text(json.dumps(dict(rows=1000000,sum_id=500000500000,sum_bucket=499500000,sum_length=256000000,sum_ascii=total,sha256=sha(d/'rows.csv'),bytes=(d/'rows.csv').stat().st_size),indent=2))

def negative_controls():
    # Exercise real runner subprocess paths without starting any service.
    refuse('a169-resume-perf-negative-sql')
    with socket.socket() as z:z.bind(('127.0.0.1',53399))
    sqlcmd=['docker','run','--rm','--cidfile',str(R/'negative-sql.cid'),'--name','a169-resume-perf-negative-sql','--label','agent.worktree='+LABEL,'--label','agent.lane=storage_perf169','--network','host','--cpuset-cpus','20-21','--cpu-shares','2','--memory','2g','--memory-swap','2g','-e','PGCONNECT_TIMEOUT=2',PG,'psql','-X','-w','-h','127.0.0.1','-p','53399','-U','postgres','-v','ON_ERROR_STOP=1','-c','SELECT 1/0']
    for tag,cmd in [('sql',sqlcmd),('docker',['docker','container','inspect','a169-resume-perf-no-such-negative-control']),('pipeline',['bash','-o','pipefail','-c','exit 7 | cat'])]:
      caught=False
      try:run(cmd,'negative-'+tag)
      except RuntimeError:caught=True
      assert caught,'fail-fast broken '+tag
    for kind in ['name','data']:
      caught=False
      try:refuse('existing-test',R if kind=='data' else None,probe=lambda _:0)
      except RuntimeError:caught=True
      assert caught,'resource refusal broken '+kind
    (R/'NEGATIVE-PASS').write_text('Real PG18.6 psql connection failure exit2, real Docker missing resource exit1, pipeline exit7 all stop runner; existing data/name refused\n')

def footprints(side):
    tag='footprint-'+side;start(side,tag)
    for j in range(3):window(tag+f'-fresh-{j}',15 if j==0 else 0,10)
    setup_data();out,_=run(loadcmd(),'footprint-load');assert 'COPY 1000000' in out;check_data()
    for j in range(3):window(tag+f'-loaded-{j}',15 if j==0 else 0,10)
    cmd=['docker','exec','-e','PGPASSWORD=pw',CLIENT,'pgbench','-h','127.0.0.1','-p',str(PORT),'-U','postgres','-n','-c','100','-j','2','-T','60','-f','/input/idle.sql','bench']
    path=R/(tag+'-100clients.log')
    with path.open('xb') as f:
      proc=S.Popen(cmd,stdout=f,stderr=S.STDOUT);t=time.monotonic()
      try:
        time.sleep(5)
        out,_=sql("SELECT count(*) FROM pg_stat_activity WHERE datname='bench' AND application_name='pgbench' AND state='idle'",'validate-100-idle');assert out.strip()=='100',out
        for j in range(3):window(tag+f'-100idle-{j}',10 if j==0 else 0,10)
        rc=proc.wait(timeout=90)
      finally:
        if proc.poll() is None:proc.terminate();proc.wait(timeout=10)
        emit(LOG,dict(tag=tag+'-100clients',cmd=cmd,exit=proc.returncode,wall=time.monotonic()-t,log=path.name))
    assert rc==0 and 'number of failed transactions: 0' in path.read_text(),path.read_text()
    check_data();stop(tag)

def manifests(tag):
    found={}
    for path in DATA.rglob('MANIFEST'):
      v=json.loads(path.read_text());found[str(path.relative_to(DATA))]=v
    (R/(tag+'.manifests.json')).write_text(json.dumps(found,indent=2));return found

def close_fixture(side):
    tag='far-close-'+side;start(side,tag)
    sql('CREATE TABLE left_data(id integer NOT NULL,bucket integer NOT NULL,payload text NOT NULL); CREATE TABLE middle_data(id integer NOT NULL,bucket integer NOT NULL,payload text NOT NULL); CREATE TABLE right_data(id integer NOT NULL,bucket integer NOT NULL,payload text NOT NULL)','create-close-tables')
    # Each middle transaction stays well below the 64MiB spill limit. Enough ordinary
    # memtable rotations cross the default L0 compaction trigger without tuning it.
    text='\n'.join(f"INSERT INTO middle_data SELECT i,i%1000,repeat(md5(i::text),8) FROM generate_series({i},{i+9999}) i;" for i in range(1,1000001,10000))
    run(psqlcmd(text),'ordinary-middle',timeout=600);check_data('middle_data');time.sleep(5)
    m=manifests(tag+'-middle')
    for table in ['left_data','right_data']:
      out,_=run(loadcmd(table),'far-load-'+table);assert 'COPY 1000000' in out;check_data(table)
    before=manifests(tag+'-before');files={str(x.relative_to(DATA)):dict(bytes=x.stat().st_size,sha256=sha(x)) for x in DATA.rglob('*.sst')}
    (R/(tag+'-files-before.json')).write_text(json.dumps(files,indent=2));x=stop(tag)
    after=manifests(tag+'-after');afterfiles={str(z.relative_to(DATA)):dict(bytes=z.stat().st_size,sha256=sha(z)) for z in DATA.rglob('*.sst')}
    (R/(tag+'-files-after.json')).write_text(json.dumps(afterfiles,indent=2))
    emit(R/'close-fixtures.jsonl',dict(side=side,before=before,after=after,middle=m,close=x,unchanged_files=[k for k,v in files.items() if afterfiles.get(k)==v],rewritten_or_removed_bytes=sum(v['bytes'] for k,v in files.items() if afterfiles.get(k)!=v)))
    # Reopen retained data in a newly owned container without setup/drop; validates clean close.
    reopen(side,tag,DATA)
    for table in ['left_data','middle_data','right_data']:check_data(table)
    stop(tag+'-reopen')

def reopen(side,tag,data):
    global ENGINE,DATA
    stable();assert ENGINE is None;DATA=P.Path(data)
    free_port()
    binary=next(x['binary'] for x in json.loads((R/'builds.json').read_text()) if x['side']==side)
    ENGINE=create('a169-resume-perf-'+tag+'-reopen','14-17','4g',IMG,[binary,'--data',str(DATA),'--listen',f'127.0.0.1:{PORT}','--password','pw','--cache-mb','2048','-c','max_connections=300'],['--user',f'{os.getuid()}:{os.getgid()}','-v',binary+':'+binary+':ro','-v',str(DATA)+':'+str(DATA),'-v','/usr/share/zoneinfo:/usr/share/zoneinfo:ro'])
    for _ in range(300):
      try:
        with socket.create_connection(('127.0.0.1',PORT),timeout=.2):break
      except OSError:time.sleep(.2)
    else:raise RuntimeError('reopen readiness timeout')

def cleanup():
    failures=[]
    for cid in list(OWN):
      try:
        z=inspect(cid);assert z['Config']['Labels']['agent.worktree']==LABEL
        if z['State']['Running']:run(['docker','stop','--time','120',cid],'cleanup-stop',timeout=150)
        run(['docker','logs',cid],'cleanup-engine')
        run(['docker','rm',cid],'cleanup-remove');OWN.remove(cid)
      except Exception:
        failures.append(dict(id=cid,error=traceback.format_exc()));traceback.print_exc()
    if failures or OWN:raise RuntimeError(dict(cleanup_failures=failures,remaining_owned_ids=OWN.copy()))

def main():
    global CLIENT
    if not (R/'NEGATIVE-PASS').exists():negative_controls()
    if sys.argv[1:] == ['negative']:return
    assert (R/'BUILD-PASS').exists();stable();input_data()
    CLIENT=create('a169-resume-perf-client','20-21','2g',PG,['sleep','infinity'],['-v',str(R/'input')+':/input:ro'])
    audit_affinity(CLIENT,'20-21',2*1024**3)
    out,_=run(['docker','exec',CLIENT,'pgbench','--version'],'pg18-client-version');assert '18.6' in out,out
    run(['bash','-c','cat /proc/self/cgroup; cat /proc/self/status; lscpu -e=CPU,CORE,SOCKET,ONLINE; df -h /home/meszmate/.cache'],'host-start')
    for work in ['copy','pgbench','index']:
      for k in range(5):
        for side in (['baseline','candidate'] if k%2==0 else ['candidate','baseline']):trial(side,work,k)
      for side in ['baseline','candidate']:trial(side,work,'limiter',True)
    for side in ['baseline','candidate']:footprints(side)
    for side in ['baseline','candidate']:close_fixture(side)
    stable();(R/'BODY-COMPLETE').write_text('all bounded planned invocations, validations, windows and closes completed; cleanup pending\n')
if __name__=='__main__':
    rc=0
    try:main()
    except BaseException:
      traceback.print_exc();(R/'RUN-FAIL').write_text(traceback.format_exc());rc=1
    finally:
      try:
        cleanup();stable()
        assert not OWN,OWN
      except BaseException:
        traceback.print_exc();rc=1
    if rc==0 and sys.argv[1:]!=['negative']:
      (R/'RUN-PASS').write_text('all bounded planned invocations, validations, windows, closes, cleanup and source/binary stability passed\n')
    sys.exit(rc)
