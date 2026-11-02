from pathlib import Path
import importlib.util,json,struct
r=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('wire','crates/pg-engine/tests/clients/wire.py');wire=importlib.util.module_from_spec(spec);spec.loader.exec_module(wire)
def drain(c):
 c.s.settimeout(15);frames=[]
 while True:
  t,b=c._msg();frames.append((t,b))
  assert t!=b'E',wire.decode(t,b)
  if t==b'Z':
   assert b==b'I',b
   return frames
def rows(frames):
 result=[]
 for t,b in frames:
  if t!=b'D':continue
  n=struct.unpack('!h',b[:2])[0];pos=2;row=[]
  for _ in range(n):
   size=struct.unpack('!i',b[pos:pos+4])[0];pos+=4
   row.append(None if size==-1 else b[pos:pos+size]);pos+=max(0,size)
  assert pos==len(b)
  result.append(row)
 return result
def simple(c,sql):
 c.query(sql);return drain(c)
def counts(c):
 got=rows(simple(c,"SELECT name,parameter_types,from_sql,generic_plans,custom_plans FROM pg_prepared_statements WHERE name='oltp_probe'"))
 assert len(got)==1,got
 a=[v.decode() for v in got[0]];assert a[:3]==['oltp_probe','{integer}','f'],a
 return list(map(int,a[3:]))
queries={'point_select':'SELECT id, owner, balance, note, created FROM accounts WHERE id = $1','range_scan_100':'SELECT id, balance FROM accounts WHERE id BETWEEN $1 AND $1 + 99 ORDER BY id'}
output=[]
for label,port in [('baseline',53110),('postgres',53100)]:
 for case,sql in queries.items():
  for mode in ['auto','force_generic_plan','force_custom_plan']:
   c=wire.Conn(port,db='perf_oltp20261102_clean_200000')
   try:
    setup=simple(c,f"SET plan_cache_mode='{mode}'")
    c.parse('oltp_probe',sql,[23]);c.describe('S','oltp_probe');c.sync();parsed=drain(c)
    assert [b for t,b in parsed if t==b't']==[struct.pack('!hI',1,23)]
    snapshots=[counts(c)]
    for i in range(20):
     g=1+(i*7919)%199900
     c.bind('','oltp_probe',[struct.pack('!i',g)],formats=[1],result_formats=[1]);c.describe('P','');c.execute('',0);c.sync();fr=drain(c);got=rows(fr)
     if case=='point_select':
      expected=[[struct.pack('!i',g),struct.pack('!i',g%50000),struct.pack('!q',g*7919%100000),f'note {g}'.encode(),struct.pack('!i',8766+g%365)]]
     else:expected=[[struct.pack('!i',j),struct.pack('!q',j*7919%100000)] for j in range(g,g+100)]
     assert got==expected,(label,case,mode,i)
     snapshots.append(counts(c))
    record={'target':label,'case':case,'mode':mode,'counters':snapshots,'parsed':[wire.decode(t,b) for t,b in parsed]};output.append(record)
    print(label,case,mode,snapshots[-1],flush=True)
   finally:c.s.close()
(r/'plan-choice-results.json').write_text(json.dumps(output,indent=2)+'\n')
