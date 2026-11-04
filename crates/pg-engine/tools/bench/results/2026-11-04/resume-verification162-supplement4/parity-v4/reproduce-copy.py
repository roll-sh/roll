#!/usr/bin/env python3
from pathlib import Path
import hashlib,json,socket,subprocess,time
v=Path(__file__).resolve().parent;c=Path('/home/meszmate/.cache/a162')
r=c/'pgr/resume-3997cf00-r4';old=c/'pgr/resume-3997cf00'
out=v/'copy-proof';out.mkdir()
identity=json.loads((v/'source-and-fixture-identity.json').read_text());binpath=Path(identity['engine_binary'])
assert hashlib.sha256(binpath.read_bytes()).hexdigest()==identity['engine_sha256']
fixture=r/'src-repo/src/test/regress/data/onek.data';oldfixture=old/'src-repo/src/test/regress/data/onek.data'
assert fixture.resolve().is_relative_to(r.resolve())
assert not oldfixture.resolve().is_relative_to(old.resolve())
assert fixture.read_bytes()==oldfixture.read_bytes()
data=fixture.read_text();assert len(data.splitlines())==1000
create='CREATE TEMP TABLE a162_copy_fixture ('+', '.join(f'c{i} text' for i in range(16))+');\n'
query="SELECT 'rows=' || count(*) FROM a162_copy_fixture;\n"
records=[]
def psql(label,command,sql):
 (out/(label+'.sql')).write_text(sql)
 run=subprocess.run(command,input=sql,capture_output=True,text=True,timeout=60)
 (out/(label+'.stdout.log')).write_text(run.stdout);(out/(label+'.stderr.log')).write_text(run.stderr)
 records.append({'label':label,'command':command,'exit':run.returncode})
 (out/'exits.json').write_text(json.dumps(records,indent=2)+'\n')
 return run
ref=psql('postgres-reference',['docker','exec','-i','a162-rref','psql','-X','-At','-v','ON_ERROR_STOP=1','-U','postgres','-d','postgres'],create+'COPY a162_copy_fixture FROM STDIN;\n'+data+'\\.\n'+query)
assert ref.returncode==0 and 'rows=1000' in ref.stdout,(ref.returncode,ref.stdout,ref.stderr)
engcmd=['docker','exec','-i','-e','PGPASSWORD=pw','a162-psql','psql','-X','-At','-v','ON_ERROR_STOP=1','-h','127.0.0.1','-p','53254','-U','postgres','-d','postgres']
try:
 with socket.create_connection(('127.0.0.1',53254),timeout=.2):
  raise RuntimeError('reproduction port already occupied; refusing to launch')
except (ConnectionRefusedError,TimeoutError):
 pass
for label,allowed,path,expected in [('original-escaping-path',old,oldfixture,3),('corrected-contained-path',r,fixture,0)]:
 datadir=r/'copy-repro'/label/'data';datadir.parent.mkdir(parents=True)
 args=[str(binpath),'--data',str(datadir),'--listen','127.0.0.1:53254','--no-sync','--idle-close-secs','100000','--allow-server-files',str(allowed)]
 (out/(label+'.engine-command.json')).write_text(json.dumps(args,indent=2)+'\n')
 with (out/(label+'.engine.log')).open('w') as log:
  proc=subprocess.Popen(args,stdout=log,stderr=subprocess.STDOUT)
  try:
   deadline=time.monotonic()+120
   while True:
    assert proc.poll() is None,'owned reproduction engine exited before ready'
    try:
     with socket.create_connection(('127.0.0.1',53254),timeout=.5):break
    except OSError:
     assert time.monotonic()<deadline,'owned reproduction engine startup timed out'
     time.sleep(.1)
   result=psql(label,engcmd,create+"COPY a162_copy_fixture FROM '"+str(path).replace("'","''")+"';\n"+query)
   assert result.returncode==expected,(label,result.returncode,result.stdout,result.stderr)
   if expected:
    assert 'absolute path not allowed' in result.stderr and 'rows=1000' not in result.stdout
   else:
    assert 'rows=1000' in result.stdout and 'ERROR' not in result.stderr
  finally:
   proc.terminate()
   try:rc=proc.wait(timeout=30)
   except subprocess.TimeoutExpired:proc.kill();rc=proc.wait(timeout=10)
   records.append({'label':label+'-engine-cleanup','pid':proc.pid,'exit':rc})
   (out/'exits.json').write_text(json.dumps(records,indent=2)+'\n')
proof={'status':'PASS','engine_source':identity['tested_source'],'engine_sha256':identity['engine_sha256'],'fixture_sha256':hashlib.sha256(fixture.read_bytes()).hexdigest(),'reference_rows':1000,'corrected_engine_rows':1000,'original_engine_exit':3,'original_error':'absolute path not allowed','original_canonical_path':str(oldfixture.resolve()),'corrected_canonical_path':str(fixture.resolve()),'old_allowed_root':str(old),'new_allowed_root':str(r),'source_or_limits_changes':False}
(out/'proof.json').write_text(json.dumps(proof,indent=2)+'\n');print(json.dumps(proof,indent=2))
