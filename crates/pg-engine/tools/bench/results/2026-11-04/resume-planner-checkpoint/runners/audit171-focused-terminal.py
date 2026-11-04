"""Read-only audit of the bounded lazy171 successor; never turns a partial run into a full pass."""
from pathlib import Path
import hashlib,json,subprocess,os,re,datetime
D=Path('/home/meszmate/.cache/pg-engine-resume-20261104/session_planner');E=D/'focused171-lazy-v1';W='/home/meszmate/roll/.claude/worktrees/agent-ad5ff4bf47e789536'
plan=json.loads((D/'focused171-lazy-v1-plan.json').read_text());source=plan['source']
def git(*a):return subprocess.check_output(['git','-C',W,*a]).decode().strip()
# A running group is not terminal, even if a wrapper has emitted a reassuring line.
live=[]
for p in Path('/proc').iterdir():
 if not p.name.isdigit():continue
 try:
  tail=(p/'stat').read_text().rsplit(')',1)[1].split()
  if int(tail[2])==1305624:live.append(int(p.name))
 except(OSError,ValueError,IndexError):pass
assert not live,('Owned process group still live',live)
wrapper=(D/'focused171-lazy-v1-wrapper.log').read_text();match=re.search(r'^HEAVY-EXIT (\d+)$',wrapper,re.M);assert match,'Missing terminal wrapper record'
head=git('rev-parse','HEAD');tree=git('rev-parse','HEAD^{tree}');clean=not git('status','--porcelain')
assert head==source and clean and tree==(E/'tree.txt').read_text().strip(),(head,tree,clean)
for line in(D/'focused171-lazy-v1-frozen.sha256').read_text().splitlines():
 h,p=line.split('  ',1);assert hashlib.sha256(Path(p).read_bytes()).hexdigest()==h,p
exits={}
for line in(E/'exits.tsv').read_text().splitlines():
 name,status=line.split('\t');assert name not in exits,name;exits[name]=int(status)
stages={};problems=[]
for name,count in plan['expected_tests'].items():
 if name not in exits:
  stages[name]=dict(status='NOTRUN after fail-fast stop');continue
 log=(E/(name+'.log')).read_text();summaries=re.findall(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;',log,re.M)
 binary_file=E/(name+'-binary.json');binary=json.loads(binary_file.read_text())if binary_file.exists()else None
 binary_matches=bool(binary and Path(binary['binary']).is_file()and hashlib.sha256(Path(binary['binary']).read_bytes()).hexdigest()==binary['sha256'])
 passed=exits[name]==0 and summaries==[('ok',str(count),'0','0')] and binary_matches
 if name=='generic':passed=passed and 'six Binds after RESET: [Int8(1), Int8(5)]'in log
 if name=='parallel':passed=passed and log.count('worker to_launch/launched delta=(2, 2)')==2
 if name=='cost-golden':passed=passed and all(re.search('^test '+n+r' \.\.\. ok$',log,re.M)for n in('golden','golden_shuffled','golden_wire'))and log.count('1 golden files match')==3
 if re.search(r'^error:|test result: FAILED|panicked at',log,re.M):passed=False
 stages[name]=dict(status='PASS'if passed else'FAILED',command_exit=exits[name],summaries=summaries,binary=binary,binary_matches=binary_matches)
 if not passed:problems.append(name)
if 'clippy'in exits:
 p=E/'clippy-summary.json';c=json.loads(p.read_text())if p.exists()else None;passed=exits['clippy']==0 and c and c.get('build_finished_success')and c.get('errors')==0
 stages['clippy']=dict(status='PASS'if passed else'FAILED',command_exit=exits['clippy'],summary=c)
 if not passed:problems.append('clippy')
else:stages['clippy']=dict(status='NOTRUN after fail-fast stop')
allpass=not problems and set(exits)==set(plan['expected_tests'])|{'clippy'} and int(match[1])==0 and all(s['status']=='PASS'for s in stages.values())
report=dict(observed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),source=head,tree=tree,clean=clean,status='FOCUSED PASS'if allpass else'FOCUSED FAILED/PARTIAL',wrapper_exit=int(match[1]),command_exits=exits,stages=stages,problems=problems,owned_process_group_absent=True,frozen_hashes_match=True,scope='Only authorized focused debug tests and libraryClippy;NOTfullgates',full_gates='NOTRUN;parentreview+decisionrequired',production_acceptance=False,performance_acceptance=False,serial_bound='Original assertion unmodified; targeted outcomes do not establish cause of prior failure',controls='startup and sampled actual compile/test descendants only;wrapper/heavy housekeeping unrestricted;no exhaustive isolation/performance claim')
(E/'TERMINAL.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps(dict(status=report['status'],source=head,exits=exits,stages={k:v['status']for k,v in stages.items()}),indent=2))
