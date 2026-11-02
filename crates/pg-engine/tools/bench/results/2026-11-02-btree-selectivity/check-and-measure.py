from pathlib import Path
from datetime import datetime,timezone
import hashlib,json,os,socket,subprocess,time
r=Path(__file__).resolve().parent
old=Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102')
def command(args):return subprocess.check_output(args,text=True).strip()
def sha(path):return hashlib.sha256(Path(path).read_bytes()).hexdigest()
record={'at_utc':datetime.now(timezone.utc).isoformat(),'source':command(['git','rev-parse','HEAD']),'production_tree':command(['git','rev-parse','HEAD:crates/pg-engine/src']),'git_status':command(['git','status','--porcelain']),'load':Path('/proc/loadavg').read_text().strip(),'runner_sha256':sha('crates/pg-engine/tools/bench/paired_oltp.py'),'client_sha256':sha(old/'final-client/bench'),'engines':{},'psql_helper':json.loads(command(['docker','inspect','t3-d4020b98-perf-pg']))[0]['Image']}
assert record['git_status']==''
for name,port in [('baseline',53210),('candidate',53211)]:
 pid=int((r/f'{name}.pid').read_text());proc=Path(f'/proc/{pid}')
 for attempt in range(120):
  try:
   with socket.create_connection(('127.0.0.1',port),timeout=1):break
  except OSError:time.sleep(.25)
 else:raise RuntimeError(name+' not listening')
 record['engines'][name]={'pid':pid,'port':port,'exe':str((proc/'exe').resolve(strict=True)),'sha256':sha(proc/'exe'),'cmdline':(proc/'cmdline').read_bytes().replace(b'\0',b' ').decode().strip(),'affinity':sorted(os.sched_getaffinity(pid)),'thread_affinities':sorted({tuple(os.sched_getaffinity(int(t.name))) for t in (proc/'task').iterdir()}),'cgroup':(proc/'cgroup').read_text()}
 assert record['engines'][name]['affinity']==list(range(28,32))
path=r/'runtime.json';assert not path.exists();path.write_text(json.dumps(record,indent=2)+'\n')
for name,port in [('baseline',53210),('candidate',53211)]:
 args=['docker','exec','-i','-e','PGPASSWORD=pw','t3-d4020b98-perf-pg','psql','-XAt','-v','ON_ERROR_STOP=1','-h','127.0.0.1','-p',str(port),'-U','postgres','-d','perf_oltp20261102_clean_200000']
 result=subprocess.run(args,input=(r/'check-plans.sql').read_text(),text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,timeout=60)
 (r/f'{name}-plans.log').write_text(result.stdout);assert result.returncode==0,result.stdout
assert (r/'baseline-plans.log').read_bytes()==(r/'candidate-plans.log').read_bytes(),'plans/settings differ'
subprocess.run(['python3',str(r/'probe-plan-choice.py')],check=True,timeout=120)
config={'bench':str(old/'final-client/bench'),'out':str(r/'candidate-vs-baseline'),'db':'perf_oltp20261102_clean_200000','rows':200000,'rounds':5,'seconds':3,'cases':{'point_select':[1,8],'range_scan_100':[1,8],'secondary_lookup':[8]},'psql_container':'t3-d4020b98-perf-pg','expected_fixture':'200000|20000100000|4999900000|9999900000|ac8ccd2a44331a8aa083be3b8e06ee5f','runtime_provenance':'runtime.json','targets':[{'name':name,'port':info['port'],'probe':{'pid':info['pid']},'source':('5b32046dee8c9908e2bb25c892ff60df54c33b33' if name=='baseline' else record['source'])} for name,info in record['engines'].items()]}
p=r/'candidate-vs-baseline.json';p.write_text(json.dumps(config,indent=2)+'\n')
subprocess.run(['python3','crates/pg-engine/tools/bench/paired_oltp.py',str(p)],check=True,timeout=600)
