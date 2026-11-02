from pathlib import Path
import subprocess,json
r=Path(__file__).resolve().parent
container='t3-d4020b98-perf-pg'
db='perf_oltp20261102_wrong_values_v2_101'
def sql(port,db,query):
 return subprocess.check_output(['docker','exec','-e','PGPASSWORD=pw',container,'psql','-XAt','-v','ON_ERROR_STOP=1','-h','127.0.0.1','-p',str(port),'-U','postgres','-d',db,'-c',query],text=True,stderr=subprocess.STDOUT)
results=[]
for target,port in [('baseline',53110),('postgres',53100)]:
 assert sql(port,'postgres',f"SELECT count(*) FROM pg_database WHERE datname='{db}'").strip()=='0'
 base=['taskset','-c','32-35',str(r/'final-client/bench'),'--target','wire','--label',target,'--port',str(port),'--db',db]
 for args in [['createdb'],['oltp-load','--rows','101']]:
  p=subprocess.run(base+args,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,timeout=120)
  (r/f'final-negative-{target}-{args[0]}.log').write_text(p.stdout)
  assert p.returncode==0,p.stdout
 # Preserve cardinalities and identities; every balance is deliberately wrong.
 (r/f'final-negative-{target}-mutation.log').write_text(sql(port,db,'UPDATE accounts SET balance = balance + 1'))
 for case in ['point_select','point_select_simple','range_scan_100','secondary_lookup']:
  p=subprocess.run(base+['oltp','--rows','101','--only',case,'--clients','1','--secs','1'],text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,timeout=40)
  (r/f'final-negative-{target}-{case}.log').write_text(p.stdout)
  assert p.returncode!=0,(target,case,p.stdout)
  assert 'errors=0' not in p.stdout and 'first_error=' in p.stdout,(target,case,p.stdout)
  results.append({'target':target,'case':case,'exit':p.returncode,'rejected':True})
(r/'final-live-negative-results.json').write_text(json.dumps(results,indent=2)+'\n')
print('Eight live wrong-value controls rejected')
