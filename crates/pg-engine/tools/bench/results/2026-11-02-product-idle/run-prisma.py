import hashlib,json,os,subprocess,sys
from pathlib import Path
config=json.loads(Path(sys.argv[1]).read_text());out=Path(sys.argv[2]);out.mkdir(exist_ok=False)
repo=Path('/home/meszmate/worktrees/roll/t3-d4020b98');harness=repo/'crates/pg-engine/tests/clients'
cache=out/'cache';(cache/'home').mkdir(parents=True);(cache/'src/prisma').mkdir(parents=True)
image='sha256:de170b39439eb0e5a3444ad7d60b015614a381e10fecae2e30ec9e735122f38d'
pgimage='sha256:898c60cb390d1085bc97bdd6a1ae968757421df394b071848b52b1c09c89d6a5'
commands=[];results={};cleanup=[]
def run(args,log,stdin=None):
 commands.append(args);(out/'commands.json').write_text(json.dumps(commands,indent=2)+'\n')
 with log.open('w') as stream:
  c=subprocess.run(args,input=stdin,text=True,stdout=stream,stderr=subprocess.STDOUT,timeout=900)
 if c.returncode:raise RuntimeError(f'command failed {c.returncode}: {log}')
for target in config['targets']:
 name='t3-d4020b98-product-prisma-'+target['name']
 setup_name=name+'-setup'
 try:
  for n in [name,setup_name]:
   found=subprocess.run(['docker','container','inspect',n],capture_output=True)
   if found.returncode==0:raise RuntimeError('container name occupied: '+n)
  base=['docker','run','--rm','--network','host','--cpus=4','--cpuset-cpus=32-35','--memory=1g','--memory-swap=1g','--label','agent.worktree=t3-d4020b98','--label','agent.task=product-prisma']
  setup=base+['--name',setup_name,'-i','-e','PGPASSWORD=pw',pgimage,'psql','-X','-v','ON_ERROR_STOP=1','-h','127.0.0.1','-p',str(target['port']),'-U','postgres','-d','postgres']
  run(setup,out/(target['name']+'-setup.log'),(harness/'suites/prisma.setup.sql').read_text())
  result_dir=cache/'results'/target['name'];result_dir.mkdir(parents=True)
  args=base+['--name',name,'--user',f'{os.getuid()}:{os.getgid()}','-e','HOME=/cache/home','-v',f'{cache}:/cache','-v',f'{harness}:/harness:ro','-e','PGHOST=127.0.0.1','-e',f'PGPORT={target["port"]}','-e','PGUSER=postgres','-e','PGPASSWORD=pw','-e','PGDATABASE=postgres','-e',f'TARGET={target["name"]}','-e','SRC=/cache/src/prisma','-e',f'OUT=/cache/results/{target["name"]}','-w','/cache/src/prisma',image,'sh','/harness/suites/prisma.sh']
  run(args,out/(target['name']+'.log'))
  collected=subprocess.check_output([sys.executable,'-I',str(harness/'collect.py'),str(result_dir)],text=True)
  (out/(target['name']+'.txt')).write_text(collected)
  lines=collected.splitlines();assert len(lines)>=47 and all(line.startswith('PASS ') for line in lines),collected
  results[target['name']]=lines
 finally:
  for n in [name,setup_name]:
   info=subprocess.run(['docker','container','inspect',n],capture_output=True,text=True)
   if info.returncode==0:
    data=json.loads(info.stdout)[0]
    assert data['Config']['Labels'].get('agent.task')=='product-prisma'
    removed=subprocess.run(['docker','rm','-f',data['Id']],capture_output=True,text=True)
    cleanup.append({'container':data['Id'],'remove_exit':removed.returncode})
   else:cleanup.append({'name':n,'already_absent':True})
  (out/'cleanup.json').write_text(json.dumps(cleanup,indent=2)+'\n')
values=list(results.values());assert all(v==values[0] for v in values)
lock=cache/'prisma-node/package-lock.json'
(out/'complete.json').write_text(json.dumps({'matching_test_ids':len(values[0]),'all_pass':True,'node_image':image,'pg_image':pgimage,'package_lock_sha256':hashlib.sha256(lock.read_bytes()).hexdigest(),'suite_sha256':hashlib.sha256((harness/'suites/prisma.sh').read_bytes()).hexdigest(),'setup_sha256':hashlib.sha256((harness/'suites/prisma.setup.sql').read_bytes()).hexdigest()},indent=2)+'\n')
print('Pinned Prisma migration/client check matches across targets:',len(values[0]),'passing IDs')
