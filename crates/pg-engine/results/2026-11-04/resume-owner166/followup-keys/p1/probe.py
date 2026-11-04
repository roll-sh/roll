from pathlib import Path
import hashlib,json,os,signal,socket,subprocess,time
out=Path(__file__).parent
root=Path('/home/meszmate/roll/.claude/worktrees/agent-ac9369af470e262d2')
binary=Path('/home/meszmate/.cache/a166/target-debug/debug/roll-engine')
expected='0f077b8d01577f92085494a7c3c23f2c62e3fb09';port=53329;db='a166_snapshot_keys_p1';events=[]
def run(name,args,stdin=None):
 p=subprocess.run(args,input=stdin,text=True,capture_output=True)
 (out/(name+'.stdout')).write_text(p.stdout);(out/(name+'.stderr')).write_text(p.stderr)
 events.append({'name':name,'argv':args,'exit':p.returncode})
 (out/'commands.json').write_text(json.dumps(events,indent=2)+'\n')
 assert p.returncode==0,(name,p.stderr)
 return p.stdout
head=run('source-head',['git','-C',str(root),'rev-parse','HEAD']).strip();assert head==expected
run('source-status',['git','-C',str(root),'status','--porcelain=v1'])
run('mounts',['docker','inspect','a166-pgh','a166-psql1'])
(out/'identities.json').write_text(json.dumps({'source_head':head,'binary':str(binary),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'runner_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'sql_sha256':hashlib.sha256((out/'probe.sql').read_bytes()).hexdigest()},indent=2)+'\n')
args=['docker','exec','-i','-e','PGPASSWORD=pw','a166-psql1','psql','-XAt','-h','127.0.0.1','-U','postgres','-v','ON_ERROR_STOP=1']
def sql(name,target,database,text):return run(name,args+['-p',str(target),'-d',database],text)
version=sql('reference-version',53320,'postgres','SHOW server_version;');assert version.startswith('18.6'),version
assert sql('reference-db-absent',53320,'postgres',f"SELECT count(*) FROM pg_database WHERE datname='{db}';").strip()=='0'
with socket.socket() as sock:assert sock.connect_ex(('127.0.0.1',port))!=0,'port occupied'
sql('create-reference-db',53320,'postgres',f'CREATE DATABASE {db};')
engine_log=(out/'engine.log').open('w')
engine=subprocess.Popen([str(binary),'--data',str(out/'engine-data'),'--listen',f'127.0.0.1:{port}','--no-sync','--idle-close-secs','60'],stdout=engine_log,stderr=subprocess.STDOUT)
events.append({'name':'engine-start','pid':engine.pid,'nice':os.getpriority(os.PRIO_PROCESS,engine.pid)})
try:
 for _ in range(150):
  assert engine.poll() is None,'engine exited'
  with socket.socket() as sock:
   if sock.connect_ex(('127.0.0.1',port))==0:break
  time.sleep(.1)
 else:raise RuntimeError('engine startup timeout')
 sql('create-engine-db',port,'postgres',f'CREATE DATABASE {db};')
 for name,target in [('reference',53320),('engine',port)]:
  transcript=sql(name,target,db,(out/'probe.sql').read_text())
  assert 'Hash Join' in transcript and 'Hash Full Join' in transcript,(name,'missing plans')
  assert '\nf\n' in transcript and '\n1\n' in transcript,(name,'missing inner result')
  assert transcript.rstrip().endswith('t\nt'),(name,'count mismatch')
 assert run('source-head-after',['git','-C',str(root),'rev-parse','HEAD']).strip()==head
 (out/'complete').write_text('Both PostgreSQL18.6 and current engine match expected results; plan shapes recorded.\n')
finally:
 if engine.poll() is None:
  engine.send_signal(signal.SIGTERM)
  try:engine.wait(timeout=30)
  except subprocess.TimeoutExpired:engine.kill();engine.wait();events.append({'name':'forced-stop-owned-engine','pid':engine.pid})
 events.append({'name':'engine-exit','exit':engine.returncode})
 sql('drop-reference-db',53320,'postgres',f'DROP DATABASE {db} WITH (FORCE);')
 (out/'commands.json').write_text(json.dumps(events,indent=2)+'\n');engine_log.close()
 with socket.socket() as sock:assert sock.connect_ex(('127.0.0.1',port))!=0,'engine listener remains'
