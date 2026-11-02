"""Run pinned, unmodified node-postgres tests, then the existing Prisma migration suite."""
import hashlib, json, os, subprocess, sys
from pathlib import Path
config=json.loads(Path(sys.argv[1]).read_text())
out=Path(sys.argv[2]); out.mkdir(exist_ok=False)
repo=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
harness=repo/'crates/pg-engine/tests/clients'
cache=out/'nodepg-cache'
(cache/'home').mkdir(parents=True); (cache/'results').mkdir()
image='sha256:de170b39439eb0e5a3444ad7d60b015614a381e10fecae2e30ec9e735122f38d'
pgimage='sha256:898c60cb390d1085bc97bdd6a1ae968757421df394b071848b52b1c09c89d6a5'
commands=[]; results={}; cleanup=[]
def run(args, log, stdin=None):
 commands.append(args); (out/'commands.json').write_text(json.dumps(commands,indent=2)+'\n')
 with log.open('w') as stream:
  result=subprocess.run(args,input=stdin,text=True,stdout=stream,stderr=subprocess.STDOUT,timeout=900)
 if result.returncode: raise RuntimeError(f'command failed {result.returncode}: {log}')
source='https://github.com/brianc/node-postgres/archive/refs/tags/pg@8.16.0.tar.gz'
run([str(harness/'fetch.sh'),str(cache/'src/nodepg'),source],out/'fetch.log')
for target in config['targets']:
 name='t3-d4020b98-product-nodepg-'+target['name']; setup_name=name+'-setup'
 try:
  for n in [name,setup_name]:
   if subprocess.run(['docker','container','inspect',n],capture_output=True).returncode==0:
    raise RuntimeError('container name occupied: '+n)
  base=['docker','run','--rm','--network','host','--cpus=4','--cpuset-cpus=32-35','--memory=1g','--memory-swap=1g','--label','agent.worktree=t3-d4020b98','--label','agent.task=product-nodepg']
  setup=base+['--name',setup_name,'-i','-e','PGPASSWORD=pw',pgimage,'psql','-X','-v','ON_ERROR_STOP=1','-h','127.0.0.1','-p',str(target['port']),'-U','postgres','-d','postgres']
  run(setup,out/(target['name']+'-setup.log'),(harness/'suites/nodepg.setup.sql').read_text())
  result_dir=cache/'results'/target['name']; result_dir.mkdir()
  args=base+['--name',name,'--user',f'{os.getuid()}:{os.getgid()}','-e','HOME=/cache/home','-v',f'{cache}:/cache','-v',f'{harness}:/harness:ro','-e','PGHOST=127.0.0.1','-e',f'PGPORT={target["port"]}','-e','PGUSER=postgres','-e','PGPASSWORD=pw','-e','PGDATABASE=postgres','-e',f'TARGET={target["name"]}','-e','SRC=/cache/src/nodepg','-e',f'OUT=/cache/results/{target["name"]}','-w','/cache/src/nodepg',image,'sh','/harness/suites/nodepg.sh']
  run(args,out/(target['name']+'.log'))
  collected=subprocess.check_output([sys.executable,'-I',str(harness/'collect.py'),str(result_dir)],text=True)
  (out/(target['name']+'.txt')).write_text(collected)
  lines=collected.splitlines(); assert len(lines)>=140 and len(set(lines))==len(lines),collected
  results[target['name']]=lines
 finally:
  for n in [name,setup_name]:
   info=subprocess.run(['docker','container','inspect',n],capture_output=True,text=True)
   if info.returncode==0:
    data=json.loads(info.stdout)[0]; assert data['Config']['Labels'].get('agent.task')=='product-nodepg'
    removed=subprocess.run(['docker','rm','-f',data['Id']],capture_output=True,text=True)
    cleanup.append({'container':data['Id'],'remove_exit':removed.returncode})
   else: cleanup.append({'name':n,'already_absent':True})
  (out/'cleanup.json').write_text(json.dumps(cleanup,indent=2)+'\n')
values=list(results.values()); assert all(v==values[0] for v in values), 'node-postgres test outcomes differ'
(out/'nodepg-complete.json').write_text(json.dumps({'matching_test_ids':len(values[0]),'counts':{kind:sum(line.startswith(kind+' ') for line in values[0]) for kind in ['PASS','FAIL','SKIP']},'node_image':image,'pg_image':pgimage,'source_url':source,'suite_sha256':hashlib.sha256((harness/'suites/nodepg.sh').read_bytes()).hexdigest(),'yarn_lock_sha256':hashlib.sha256((cache/'src/nodepg/yarn.lock').read_bytes()).hexdigest()},indent=2)+'\n')
print('node-postgres outcomes match:',len(values[0]),flush=True)
run([sys.executable,str(out.parent.parent/'run-prisma.py'),sys.argv[1],str(out/'prisma')],out/'prisma.log')
(out/'complete.json').write_text(json.dumps({'nodepg':'matching','prisma':'matching'},indent=2)+'\n')
