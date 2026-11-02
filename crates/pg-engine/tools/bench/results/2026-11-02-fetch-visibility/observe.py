"""Check the all-visible premise on a private baseline fixture before changing code."""
from pathlib import Path
import importlib.util,json,subprocess,socket,time,shutil,sys
sys.dont_write_bytecode=True
ROOT=Path(__file__).resolve().parent;OUT=ROOT/'fixture-observation';OUT.mkdir(exist_ok=False)
DIAG=Path('/home/meszmate/.cache/pg-engine-perf-20261102/planning-diagnostic/diagnose.py')
s=importlib.util.spec_from_file_location('diag',DIAG);d=importlib.util.module_from_spec(s);s.loader.exec_module(d)
wire=d.load_module('wire',d.WIRE);engine=None;data=OUT/'data';before=d.tree_manifest(d.FIXTURE)
def save(name,x):
 with (OUT/name).open('x') as f:json.dump(x,f,indent=2);f.write('\n')
def check(x,m):
 if not x:raise RuntimeError(m)
try:
 check(d.sha(d.ENGINE)==d.HASHES[d.ENGINE],'baseline hash')
 d.require_stopped(d.FIXTURE);d.command(['cp','-a','--reflink=auto',d.FIXTURE,data]);check(d.tree_manifest(data)==before,'copy differs')
 port=53680
 with socket.socket() as sk:sk.bind(('127.0.0.1',port))
 argv=['taskset','-c','28-31',str(d.ENGINE),'--data',str(data),'--listen',f'127.0.0.1:{port}','--password','pw','--cache-mb','2048','-c','autovacuum=off']
 with (OUT/'server.log').open('x') as log:engine=subprocess.Popen(argv,stdout=log,stderr=subprocess.STDOUT)
 save('owned.json',{'pid':engine.pid,'argv':argv})
 for i in range(120):
  check(engine.poll() is None,'engine stopped')
  try:
   with socket.create_connection(('127.0.0.1',port),timeout=.5):break
  except OSError:time.sleep(.25)
 else:raise RuntimeError('not ready')
 c=wire.Conn(port,db=d.DB);c.s.settimeout(30)
 try:
  d.query(c,'CREATE EXTENSION IF NOT EXISTS pg_visibility')
  sql="SELECT pg_relation_size('accounts'::regclass)/8192, all_visible, all_frozen FROM pg_visibility_map_summary('accounts'::regclass)"
  initial=d.text_rows(d.query(c,sql));save('visibility-before.json',initial)
  pre=d.preflight(wire,port,'auto');save('preflight.json',pre)
  final=d.text_rows(d.query(c,sql));save('visibility-after.json',final)
 finally:c.s.close()
 check(d.tree_manifest(d.FIXTURE)==before,'original fixture changed')
 save('result.json',{'baseline_sha256':d.sha(d.ENGINE),'visibility_columns':['heap_pages','all_visible','all_frozen'],'before':initial,'after':final,'all_visible_fast_path_relevant':all(int(row[1])>=int(row[0]) for row in initial),'original_fixture_unchanged':True,'production_edited':False})
 print(json.dumps(json.loads((OUT/'result.json').read_text()),indent=2))
finally:
 cleanup={}
 if engine is not None:
  try:d.stop(engine);cleanup={'pid':engine.pid,'exit':engine.returncode,'stopped':True}
  except Exception as error:cleanup={'pid':engine.pid,'stopped':engine.poll() is not None,'error':repr(error)}
 save('cleanup.json',cleanup)
 if data.exists() and cleanup.get('stopped') and not data.is_symlink() and data.parent==OUT:shutil.rmtree(data)
 check(not data.exists(),'private data not removed')
