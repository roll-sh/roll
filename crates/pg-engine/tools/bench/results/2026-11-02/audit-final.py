from pathlib import Path
import csv,hashlib,json,re,subprocess
r=Path(__file__).resolve().parent;repo=Path.cwd();source='5b32046dee8c9908e2bb25c892ff60df54c33b33';gates=r/'final-gates';base=r.parent/'final-regress-7f35a185-release';current=r/'final-regress-5b32046d-release'
def status(p,names):
 rows=list(csv.reader((p/'status.tsv').open(),delimiter='\t'));assert rows==[[n,'0'] for n in names],(p,rows);return dict(rows)
def table(p):
 text=(p/'report/results.tsv').read_text();assert '# engine_build=release' in text
 rows=list(csv.DictReader((x for x in text.splitlines() if not x.startswith('#')),delimiter='\t'));assert len(rows)==230
 assert sum(int(x['units']) for x in rows)==51150;assert all(x['engine_status']=='ok' and x['panics']=='0' for x in rows)
 return {x['file']:x for x in rows}
assert (gates/'commit.txt').read_text().strip()==source
assert (gates/'git-status.txt').read_text()==''
assert subprocess.check_output(['git','diff',source,'--','crates/pg-engine/src','crates/pg-engine/tests','Cargo.toml','Cargo.lock','crates/pg-engine/Cargo.toml'])==b''
assert subprocess.check_output(['git','diff','89e863fc','--','crates/pg-engine/tools/bench/bench.rs','crates/pg-engine/tools/bench/paired_oltp.py'])==b''
result={'source':source,'profile':'release','gates':status(gates,['build','test','clippy','pg-unit'])}
summaries=re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored',(gates/'test.log').read_text());assert summaries and all(x[0]=='ok' and x[2]=='0' for x in summaries)
result['crate_tests']=dict(zip(['passed','failed','ignored'],[sum(int(x[i]) for x in summaries) for i in range(1,4)]));result['crate_tests']['completed_targets']=len(summaries)
for p in [base,current]:status(p,['regress'])
a,b=table(base),table(current);assert a==b
ha={p.name:p.read_bytes() for p in (base/'report/hunks').glob('*.diff')};hb={p.name:p.read_bytes() for p in (current/'report/hunks').glob('*.diff')};assert ha==hb
result['regression']={'baseline_source':'7f35a185','files':230,'units':51150,'matched':sum(int(x['match']) for x in b.values()),'panics':0,'non_ok_engine_statuses':0,'added_differences':0,'hunks_byte_identical':True,'differing_units':{k:x['differing'] for k,x in b.items() if x['differing']}}
result['hunk_sha256']={k:hashlib.sha256(v).hexdigest() for k,v in hb.items()}
result['clippy_summaries']=[x for x in (gates/'clippy.log').read_text().splitlines() if 'generated ' in x and 'warning' in x]
result['pg_unit_tail']=(gates/'pg-unit.log').read_text().splitlines()[-12:]
manifest=json.loads((r/'histogram-bin/manifest.json').read_text());assert manifest['source']==source
for p in [r/'histogram-bin/roll-engine',repo/'target/release/roll-engine']:assert hashlib.sha256(p.read_bytes()).hexdigest()==manifest['binary_sha256']
result['measured_executable_sha256']=manifest['binary_sha256'];result['rust_tree_matches_measured_source']=True
subprocess.run(['python3',str(r/'audit-results.py')],check=True,stdout=subprocess.DEVNULL)
result['measurement_audit']=json.loads((r/'completed-evidence-audit.json').read_text());result['docker_recovery_e2e']='Waived; retained parent baseline12passed/4WALHTTPtimeouts; PostgreSQL units passed'
(r/'final-verification.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
