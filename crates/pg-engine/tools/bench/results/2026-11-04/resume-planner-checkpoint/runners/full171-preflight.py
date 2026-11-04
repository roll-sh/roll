"""Freeze inherited test environment and reconcile only the two test additions before launch."""
from pathlib import Path
import os,json,re,subprocess,hashlib
D=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner');W=Path('/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536');S='cac0933bc4eb5ef87674889542cab271de4615d0';B='c8618834caf45bc0a34cf9985c07f9bc13b498ef'
def git(*a):return subprocess.check_output(['git','-C',str(W),*a]).decode().strip()
assert git('rev-parse','HEAD')==S and git('rev-parse','HEAD^{tree}')=='bd48e5074887125072d7c23cb09c78fe6fe15c0b' and not git('status','--porcelain')
allpaths=git('diff','--name-only',B,S).splitlines();assert len(allpaths)==4
cargo_changed=[p for p in allpaths if Path(p).name in('Cargo.toml','Cargo.lock')or p.startswith('.cargo/')];assert not cargo_changed
literal_names=set()
for p in(W/'crates/pg-engine').rglob('*.rs'):
 if any(x in p.parts for x in('target','node_modules')):continue
 literal_names.update(re.findall(r'env::var(?:_os)?\s*\(\s*"([A-Z0-9_]+)"',p.read_text()))
prefixes=('CRASH_','STRESS_','MEMLOAD_','DEEP_','GOLDEN_','LOCALE_','DIFFTEST_','PGREGRESS_','PGDUMP_','PGISOLATION_','PGCONTRIB_','PGVECTOR_','APPCONC_','WIRE_','RESTART_','PK_PARITY_','DEPARSE_','ROLL_E2E','RUST_TEST_','CARGO_PROFILE_')
keys=sorted(k for k in os.environ if k in literal_names or k.startswith(prefixes))
allowed={'GOLDEN_FILTER','GOLDEN_RECORD'}
blockers=[k for k in keys if k not in allowed]
env=dict(inherited_test_config_keys=keys,blocker_keys=blockers,explicitly_unset_before_gate=['GOLDEN_FILTER','GOLDEN_RECORD'],default_crash_config=dict(CRASH_SEEDS='unset;source default [1]',CRASH_ROUNDS='unset;source default3'),scope='ordinary default test suite only;no configuredstress/diagnostic/SSI/storageprobe')
(D/'full171-inherited-environment.json').write_text(json.dumps(env,indent=2)+'\n');assert not blockers,blockers
expected=json.loads((D/'ind165-v3-expected-per-binary.json').read_text())
for profile in('debug','release'):
 actual=json.loads((D/'check165ind-v3'/f'{profile}-test-summary.json').read_text());assert actual['per_binary']==expected and actual['summary']==dict(passed=455,failed=0,ignored=18,summaries=61)
changes={}
for path,old_count,new_count,new_name in [('tests/kept_plans.rs',7,8,'generic_cost_uses_choice_context_after_parse'),('tests/parallel_scan.rs',9,10,'parallel_workers_use_execution_context_after_parse')]:
 p='crates/pg-engine/'+path
 old=subprocess.check_output(['git','-C',str(W),'show',B+':'+p]).decode();new=(W/p).read_text()
 pattern=r'#\[test\]\s*fn\s+([A-Za-z0-9_]+)'
 before=re.findall(pattern,old);after=re.findall(pattern,new)
 assert len(before)==old_count and len(after)==new_count and set(after)-set(before)=={new_name}and set(before)<=set(after),(path,before,after)
 assert expected[path]==[old_count,0,0];expected[path]=[new_count,0,0]
 changes[path]=dict(before=before,after=after,added=[new_name],removed=[])
summary=dict(passed=sum(v[0]for v in expected.values()),failed=sum(v[1]for v in expected.values()),ignored=sum(v[2]for v in expected.values()),summaries=len(expected));assert summary==dict(passed=457,failed=0,ignored=18,summaries=61)
(D/'full171-expected-per-binary.json').write_text(json.dumps(expected,indent=2)+'\n')
(D/'full171-count-reconciliation.json').write_text(json.dumps(dict(source=S,tree=git('rev-parse','HEAD^{tree}'),baseline=B,baseline_map='ind165-v3-expected-per-binary.json;actualc861debug+release map exact',only_test_changes=changes,Cargo_changes=cargo_changed,net_paths=allpaths,expected=summary,unfiltered_commands=['cargo test -p pg-engine','cargo test --release -p pg-engine'],other_default_test_counts_unchanged=True),indent=2)+'\n')
print(json.dumps(dict(source=S,expected=summary,inherited_blockers=blockers,Cargo_changes=cargo_changed),indent=2))
