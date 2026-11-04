from pathlib import Path
import hashlib,json,re
P=Path(__file__).parent;R1=P.parent/'combined-pg18-oracles-r1'
commands=json.loads((P/'commands.json').read_text());by={c['tag']:c for c in commands};run=json.loads((P/'run-result.json').read_text())
def outputs(tag):
 c=by[tag];return Path(c['stdout']).read_text(),Path(c['stderr']).read_text()
def objects(out):
 return [json.loads(l) for l in out.splitlines() if l.startswith('{"tag"')]
def errors(err):
 result=[]
 for block in re.split(r'(?=psql:<stdin>:\d+: ERROR:  )',err):
  m=re.search(r'ERROR:  ([0-9A-Z]{5}): (.*)',block)
  if not m:continue
  item={'sqlstate':m[1],'message':m[2],'raw':block.strip()}
  for label,key in [('DETAIL','detail'),('HINT','hint'),('CONTEXT','context'),('SCHEMA NAME','schema'),('TABLE NAME','table'),('CONSTRAINT NAME','constraint'),('LOCATION','location')]:
   match=re.search(r'^'+label+r':  (.*)(?:\n(?![A-Z ]+:|psql:)([^\n]+))?',block,re.M)
   if match:item[key]=match[1]+(('\n'+match[2]) if match[2] else '')
  result.append(item)
 return result
cases=json.loads((P/'cases.json').read_text());observations={};assertions=[]
def check(name,actual,expected):
 assertions.append({'name':name,'actual':actual,'expected':expected,'pass':actual==expected})
engine_path=Path('/home/meszmate/.cache/pg-engine-resume-20261104/combined-proof-r2/candidate2.log')
source_hashes=json.loads((P/'source-runner-hashes.json').read_text())['sha256'];engine_hash=hashlib.sha256(engine_path.read_bytes()).hexdigest();check('sealed engine raw log hash unchanged',engine_hash,source_hashes[str(engine_path)])
engine=engine_path.read_text();traces={}
for match in re.finditer(r'combined_index_build_validation (Include\(\d+\)|Duplicate\(\d+\)|Minimum\((?:false|true)\)) workers=(0|2) estimated=(false|true): Trace \{(.*?)(?=\ncombined_index_build_validation |\nok\n|\n\ntest result:)',engine,re.S):
 case,workers,estimated,body=match.groups()
 outcome=body[body.rindex('    outcome:'):]
 item={}
 for key in ['code','message','detail','hint']:
  m=re.search(r'\b'+key+r': (?:Some\(\s*)?("(?:[^"\\]|\\.)*")',outcome)
  item['sqlstate' if key=='code' else key]=json.loads(m[1]) if m else None
 traces[(case,int(workers),estimated)]=item
for c in cases:
 name=c['name'];out,err=outputs(name);objs=objects(out);es=errors(err)
 results=[{'tag':m[1],'sqlstate':m[2],'error':m[3]=='true'} for m in re.finditer(r'^RESULT (\S+) ([0-9A-Z]{5}) (true|false)$',out,re.M)]
 obs={'kind':c['kind'],'psql_actual_exit':by[name]['actual_exit'],'results':results,'errors':es,'observations':objs,'stdout':by[name]['stdout'],'stderr':by[name]['stderr']};observations[name]=obs
 check(name+' psql exit',obs['psql_actual_exit'],0)
 if c['kind']=='build':
  if name.startswith('minimum_'):case='Minimum('+('true' if 'desc' in name else 'false')+')'
  elif name.startswith('include_'):case=f"Include({name.split('_')[1]})"
  else:case=f"Duplicate({name.split('_')[1]})"
  check(name+' one result',len(results),1);check(name+' one error',len(es),1)
  obs['engine_comparisons']=[]
  for estimated in ['false','true']:
   baseline=traces.get((case,c['workers'],estimated));obs['engine_comparisons'].append({'estimated':estimated,'engine_error':baseline})
   check(name+f' engine trace found estimated={estimated}',baseline is not None,True)
   if baseline and es:
    for field in ['sqlstate','message','detail','hint']:check(name+f' {field} estimated={estimated}',es[0].get(field),baseline.get(field))
  check(name+' 6000 prebuild',[o['count'] for o in objs if o['tag']=='prebuild_count'],[6000])
  if name.startswith('minimum_'):
   expected_tid='(1,115)' if 'desc' in name else '(3,45)'
   expected_key=20 if 'desc' in name else -20
   chosen=[o for o in objs if o.get('key')==expected_key]
   check(name+' selected payload bytes',[o['actual_payload_bytes'] for o in chosen],[2880])
   check(name+' selected physical tid',[o['ctid'] for o in chosen],[expected_tid])
 elif c['kind']=='stats':
  boundaries={o['boundary']:o['counters'] for o in objs if o['tag']=='stats'};states={o['boundary']:o for o in objs if o['tag']=='state'}
  pairs=[('single','initial',[1,0,0,0,1,1]),('multi','single',[2,0,0,0,2,2]),('small','multi',[1,0,0,1,0,1]),('update','before_update',None),('upsert','before_upsert',None),('speculative','before_speculative',[1,0,0,0,1,1])]
  obs['deltas']={}
  for after,before,expected in pairs:
   delta=[a-b for a,b in zip(boundaries[after],boundaries[before])];obs['deltas'][after]=delta
   if expected is not None:check(name+' '+after+' source asserted delta',delta,expected)
   else:check(name+' '+after+' INSERT counters unchanged',[delta[0],delta[5]],[0,0])
   check(name+' '+after+' retained state count',states[after]['count'],1 if after in ['single','multi'] else 2)
   check(name+' '+after+' rows remain small',all(row['payload']=='small' for row in states[after]['rows']),True)
  check(name+' statement SQLSTATEs',[r['sqlstate'] for r in results],['54000','54000','00000','54000','54000','54000'])
 elif c['kind']=='encoding':
  pre=[o for o in objs if o['tag']=='encoding_prebuild'][0];storage=[o['storage'] for o in objs if o['tag']=='column_storage']
  check(name+' database encoding',pre['server_encoding'],c['encoding']);check(name+' client encoding',pre['client_encoding'],'UTF8');check(name+' all16 PLAIN',storage,['p']*16)
  check(name+' length',pre['length'],120);check(name+' octet length',pre['octet_length'],120 if c['encoding']=='LATIN1' else 240)
  check(name+' build result',[r['sqlstate'] for r in results],['00000' if c['encoding']=='LATIN1' else '54000'])
  if c['encoding']=='UTF8':check(name+' actual error message',es[0]['message'],'index row size 3920 exceeds btree version 4 maximum 2704 for index "ce_i"')
for name in ['171-parse-bind-generic','171-parse-bind-parallel']:
 out,err=outputs(name)
 obs={'psql_actual_exit':by[name]['actual_exit'],'raw_errors':err,'stdout':by[name]['stdout'],'stderr':by[name]['stderr'],'stdin_sha256':by[name]['stdin_sha256']};observations[name]=obs
 check(name+' one whole file invocation',len([c for c in commands if c.get('stdin_path')==str(P/(name+'.sql'))]),1)
 check(name+' actual psql exit',by[name]['actual_exit'],0)
 if name.endswith('generic'):
  obs['returned_ones']=len(re.findall(r'^ b\s*\n-+\n 1\n\(1 row\)',out,re.M));check(name+' six b=1',obs['returned_ones'],6)
  pairs=[list(map(int,m)) for m in re.findall(r'^\s+(\d+)\s*\|\s*(\d+)\s*$',out,re.M)]
  obs['generic_custom_plan_counts']=pairs;check(name+' generic/custom',pairs,[[1,5]])
 else:
  obs['returned_3000000']=len(re.findall(r'^\s*a\s*\n-+\n\s*3000000\n\(1 row\)',out,re.M));check(name+' two named statements return one a=3000000 each',obs['returned_3000000'],2)
  pairs=[list(map(int,m)) for m in re.findall(r'^\s+(\d+)\s*\|\s*(\d+)\s*$',out,re.M)]
  obs['planned_launched_deltas']=pairs;check(name+' planned/launched deltas',pairs,[[2,2],[2,2]])
  obs['gather_plans']=len(re.findall(r'^ Gather ',out,re.M));obs['workers_planned_2']=len(re.findall(r'^\s+Workers Planned: 2$',out,re.M))
  check(name+' Gather plans',obs['gather_plans'],2);check(name+' Workers Planned2',obs['workers_planned_2'],2)
check('container removal actual exit',run['container_remove_actual_exit'],0);check('container confirmed absent',run['container_absence_confirmed'],True)
check('R1 postflight93 all unchanged',json.loads((P/'r1-manifest-postflight.json').read_text())['all_match'],True)
for name,expected in json.loads((P/'copied-sql-sha256.json').read_text()).items():check('frozen SQL bytes '+name,hashlib.sha256((P/name).read_bytes()).hexdigest(),expected)
report={'job':run,'command_count':len(commands),'assertion_count':len(assertions),'failed_assertions':[a for a in assertions if not a['pass']],'observations':observations,'assertions':assertions,'scope_limit':'Only requested PostgreSQL reference outcomes. Original setting2 alone is not worker proof. Planner launched_delta is independent actual worker evidence. No broader engine parity, source acceptance, performance or TOAST closure.'}
(P/'report.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps({'commands':len(commands),'assertions':len(assertions),'failed_assertions':report['failed_assertions'],'report':str(P/'report.json')},indent=2))
