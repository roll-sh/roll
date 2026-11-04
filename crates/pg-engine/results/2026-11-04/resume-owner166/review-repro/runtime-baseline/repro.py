from pathlib import Path
import hashlib,json,os,signal,socket,subprocess,time
out=Path(__file__).parent
root=Path('/home/meszmate/roll/.claude/worktrees/agent-ac9369af470e262d2')
binary=Path('/home/meszmate/.cache/a169/target-base/release/roll-engine')
port=53329
db='a166_catalog_review_repro'
events=[]
def run(name,args,stdin=None):
    p=subprocess.run(args,input=stdin,text=True,capture_output=True)
    (out/(name+'.stdout')).write_text(p.stdout);(out/(name+'.stderr')).write_text(p.stderr)
    events.append({'name':name,'command':args,'exit':p.returncode,'stdout':str(out/(name+'.stdout')),'stderr':str(out/(name+'.stderr'))})
    return p
def client(name,target,sql):
    args=['docker','exec','-i','a166-psql1','psql','-X','-a','-h','127.0.0.1','-p',str(target),'-U','postgres','-d',db,'-v','ON_ERROR_STOP=0']
    return run(name,args,sql)
run('source-head',['git','-C',str(root),'rev-parse','HEAD'])
run('source-status',['git','-C',str(root),'status','--porcelain=v1'])
(out/'binary.sha256').write_text(hashlib.sha256(binary.read_bytes()).hexdigest()+'  '+str(binary)+'\n')
with socket.socket() as s: assert s.connect_ex(('127.0.0.1',port))!=0, 'owned scratch port taken'
admin=['docker','exec','a166-psql1','psql','-XAt','-h','127.0.0.1','-p','53320','-U','postgres','-d','postgres','-v','ON_ERROR_STOP=1','-c']
assert run('create-reference-db',admin+[f'CREATE DATABASE {db}']).returncode==0
engine_log=(out/'engine.log').open('w')
args=[str(binary),'--data',str(out/'engine-data'),'--listen',f'127.0.0.1:{port}','--no-sync','--idle-close-secs','60']
engine=subprocess.Popen(args,stdout=engine_log,stderr=subprocess.STDOUT)
events.append({'name':'engine-start','pid':engine.pid,'command':args,'nice':os.getpriority(os.PRIO_PROCESS,engine.pid)})
try:
    for i in range(150):
        if engine.poll() is not None:raise RuntimeError('engine died')
        with socket.socket() as s:
            if s.connect_ex(('127.0.0.1',port))==0:break
        time.sleep(.1)
    else:raise RuntimeError('engine not listening')
    assert run('create-engine-db',['docker','exec','a166-psql1','psql','-XAt','-h','127.0.0.1','-p',str(port),'-U','postgres','-d','postgres','-v','ON_ERROR_STOP=1','-c',f'CREATE DATABASE {db}']).returncode==0
    sql=(out/'repro.sql').read_text()
    for name,target in [('reference',53320),('engine',port)]:client(name,target,sql)
finally:
    engine.send_signal(signal.SIGTERM)
    try:engine.wait(timeout=30)
    except subprocess.TimeoutExpired:engine.kill();engine.wait();events.append({'name':'engine-stop','forced_owned_pid':engine.pid})
    events.append({'name':'engine-exit','exit':engine.returncode})
    run('drop-reference-db',admin+[f'DROP DATABASE {db} WITH (FORCE)'])
    (out/'commands.json').write_text(json.dumps(events,indent=2)+'\n')
    engine_log.close()
