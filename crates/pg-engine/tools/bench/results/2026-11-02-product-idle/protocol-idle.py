import importlib.util,json,socket,struct,sys,time
from pathlib import Path
sys.dont_write_bytecode=True
spec=importlib.util.spec_from_file_location('idle','/home/meszmate/worktrees/roll/t3-d4020b98/crates/pg-engine/tools/bench/idle.py');idle=importlib.util.module_from_spec(spec);spec.loader.exec_module(idle)
wire=idle.wire
socket.setdefaulttimeout(10)
config=json.loads(Path(sys.argv[1]).read_text())
results={}

def fatal(c):
 c.s.settimeout(8)
 kind,body=c._msg()
 assert kind==b'E',(kind,body)
 result=wire.decode(kind,body)
 try:c._msg();raise AssertionError('fatal did not close connection')
 except EOFError:pass
 return result

def transaction_query(c, sql):
 c.query(sql)
 while True:
  kind, body = c._msg()
  assert kind != b'E', wire.decode(kind, body)
  if kind == b'Z':
   assert body == b'T', body
   return

def drain(c):
 out=[]
 while True:
  kind,body=c._msg();assert kind!=b'E',wire.decode(kind,body)
  out.append(wire.decode(kind,body))
  if kind==b'Z':assert body==b'I';return out

for target in config['targets']:
 held=[];out={}
 def connect():
  c=idle.connect(target,config['db']);held.append(c);return c
 try:
  admin=connect()
  idle.query(admin,'CREATE TABLE idle_timeout_rollback (id integer)')
  victim=connect();assert idle.query(victim,'SELECT 1')==([23],[[b'1']])
  time.sleep(3)
  with socket.create_connection(('127.0.0.1',target['port'])) as cancel:
   cancel.sendall(struct.pack('!iiii',16,80877102,victim.pid,victim.secret))
  time.sleep(0.1)
  assert idle.query(victim,'SELECT 1')==([23],[[b'1']])
  victim.parse('idle_p','SELECT $1::int4',[23]);victim.bind('','idle_p',[struct.pack('!i',1)],[1],[1]);victim.describe('P','');victim.execute('');victim.sync()
  out['cancel_then_extended']=drain(victim)
  assert 'D 00000001' in out['cancel_then_extended']
  time.sleep(3)
  assert idle.query(admin,f'SELECT pg_terminate_backend({victim.pid})')==([16],[[b't']])
  out['terminate']=fatal(victim)
  assert "C='57P01'" in out['terminate'],out
  for setting,code in [('idle_session_timeout','57P05'),('idle_in_transaction_session_timeout','25P03'),('transaction_timeout','25P04')]:
   c=connect();idle.query(c,f"SET {setting} = '4s'")
   if setting!='idle_session_timeout':
    transaction_query(c,'BEGIN');transaction_query(c,'INSERT INTO idle_timeout_rollback VALUES (1)')
   else:idle.query(c,'SELECT 1')
   out[setting]=fatal(c);assert f"C='{code}'" in out[setting],out
   assert idle.query(admin,'SELECT count(*) FROM idle_timeout_rollback')==([20],[[b'0']])
  c=connect();body=b'SELECT 1\0';msg=b'Q'+struct.pack('!i',len(body)+4)+body
  c.s.sendall(msg[:1]);time.sleep(3);c.s.sendall(msg[1:]);out['fragmented_query']=drain(c)
  idle.query(admin,'UPDATE idle_probe_v1 SET balance = balance + 1 WHERE id = 1')
  try:idle.check(admin);raise AssertionError('wrong fixture passed')
  except RuntimeError as error:assert str(error)=='idle fixture types, ordering, or values differ'
  idle.query(admin,'UPDATE idle_probe_v1 SET balance = balance - 1 WHERE id = 1');idle.check(admin)
  out['negative_fixture_rejected']=True
  results[target['name']]=out
  Path(sys.argv[2]).write_text(json.dumps(results,indent=2)+'\n')
 finally:idle.close(held)
values=list(results.values());assert all(value==values[0] for value in values),results
print('All idle timeout/cancel/termination/fragment transcripts match')
