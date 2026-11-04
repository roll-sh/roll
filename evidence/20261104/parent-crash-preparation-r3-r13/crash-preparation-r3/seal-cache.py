# Cache-only sealing: text/source identity, read-only Git objects and SHA256.
# Does not apply any patch or run Rust/compiler/parser-control/engine code.
from pathlib import Path
import difflib, hashlib, json, subprocess
p=Path(__file__).parent
r1=p.parent/'executor278-crash14-preparation-r1'
r2=p.parent/'executor278-crash14-preparation-r2'
def sha(data): return hashlib.sha256(data).hexdigest()
def write_json(name,value): (p/name).write_text(json.dumps(value,indent=2)+'\n')
immutable=[]
for previous in [r1,r2]:
 listed=set()
 for line in (previous/'SHA256SUMS').read_text().splitlines():
  expected,name=line.split('  ',1)
  if sha((previous/name).read_bytes()) != expected: raise RuntimeError('immutable hash mismatch: '+str(previous/name))
  listed.add(name)
 actual={str(f.relative_to(previous)) for f in previous.rglob('*') if f.is_file() and f.name!='SHA256SUMS'}
 if listed!=actual: raise RuntimeError('immutable membership mismatch')
 immutable.append({'directory':str(previous),'payloads':len(listed),'sha256sums_sha256':sha((previous/'SHA256SUMS').read_bytes()),'all_payloads_verified_unchanged':True})
base=json.loads((r1/'source-patch-manifest.json').read_text())
proc=subprocess.Popen(['git','cat-file','--batch'],stdin=subprocess.PIPE,stdout=subprocess.PIPE)
def blob(oid):
 proc.stdin.write((oid+'\n').encode());proc.stdin.flush()
 header=proc.stdout.readline().decode().strip().split()
 if len(header)!=3 or header[1] not in ('blob','commit'):raise RuntimeError(header)
 size=int(header[2]);data=proc.stdout.read(size)
 if proc.stdout.read(1)!=b'\n':raise RuntimeError('object framing')
 return data
if not blob(base['base_commit']).decode().startswith('tree '+base['base_tree']+'\n'):raise RuntimeError('tree mismatch')
original=(p/'original/crates/pg-engine/tests/crash_random.rs').read_bytes()
if blob(base['base_harness_git_blob'])!=original:raise RuntimeError('original harness mismatch')
for entry in base['production_inputs']:
 data=blob(entry['git_blob'])
 if len(data)!=entry['bytes'] or sha(data)!=entry['sha256']:raise RuntimeError('input identity mismatch: '+entry['path'])
provenance=json.loads((p/'filesystem-provenance.json').read_text())
plan=(p/'original-failure-plan.json').read_bytes()
if blob(provenance['original_evidence_commit']+':'+provenance['original_plan_path'])!=plan:raise RuntimeError('sealed failure plan mismatch')
proc.stdin.close();proc.wait()
if proc.returncode:raise RuntimeError('cat-file failed')
# Source-text inspection only; this is not a parser control or compilation.
s=(p/'candidate/crates/pg-engine/tests/crash_random.rs').read_text();o=original.decode()
for start,end in [('fn append_log(', '/// The model after'),('fn replay(', 'const SCHEMA:'),('fn row_diff(', '/// The invariants,'),('fn step(', '/// The steps log:')]:
 if s[s.index(start):s.index(end,s.index(start))]!=o[o.index(start):o.index(end,o.index(start))]:raise RuntimeError('text drift: '+start)
assert '#[derive(Clone, Default, Debug)]\nstruct Model' in s
assert 'diagnostic::flags(original_read)' in s
assert 'after-original-check-pass' not in s
assert 'assert_eq!(out.status.signal()' not in s
assert 'Model' in (p/'assemble-cache.py').read_text() and '#[derive(Clone, Default, Debug)]' in (p/'assemble-cache.py').read_text()
paths=['crates/pg-engine/tests/crash_random.rs','crates/pg-engine/tests/support/crash14_diagnostic.rs']
changes=[];patch=[]
for name in paths:
 before=(p/'original'/name).read_bytes() if (p/'original'/name).exists() else b''
 after=(p/'candidate'/name).read_bytes()
 changes.append({'path':name,'before_sha256':sha(before) if before else None,'after_sha256':sha(after),'before_bytes':len(before),'after_bytes':len(after)})
 patch.append('diff --git a/'+name+' b/'+name+'\n')
 if not before:patch.append('new file mode 100644\n')
 patch.extend(difflib.unified_diff(before.decode().splitlines(True),after.decode().splitlines(True),fromfile='a/'+name if before else '/dev/null',tofile='b/'+name))
(p/'diagnostic-test-only.patch').write_text(''.join(patch))
patch_hash=sha((p/'diagnostic-test-only.patch').read_bytes())
reviews=[{'path':name,'sha256':sha((p/name).read_bytes()),'taskId':json.loads((p/name).read_text())['taskId']} for name in ['independent-review-r1.json','independent-review-r2.json']]
base.update({'status':'PREPARATION_ONLY_UNCOMPILED_UNFORMATTED_UNRUN','revision':'preparation-r3','diagnostic_revision':'preparation-r3','event_schema':3,
 'patch_sha256':patch_hash,'test_only_changes':changes,'independent_reviews':reviews,'immutable_prior_preparations':immutable,
 'parent_r1_manifest_sha256':sha((r1/'source-patch-manifest.json').read_bytes()),'parent_r2_manifest_sha256':sha((r2/'source-patch-manifest.json').read_bytes()),
 'input_verification':'Read-only exact commit/tree/harness and 882 production/build-input blob-byte checks; no compiled-workspace claim',
 'embedded_test_identity':'include_bytes of both test files, hashed at begin; exact after_sha256/after_bytes compared to manifest; no hardcoded circular self-hash',
 'required_future_profile':'release, matching sealed original CRASH crash30 command',
 'runtime_invocation':'preverified release test executable DIRECTLY; no Cargo within runtime deadline',
 'original_failure_plan':{'path':'original-failure-plan.json','sha256':sha(plan),'evidence_commit':provenance['original_evidence_commit'],'git_path':provenance['original_plan_path']},
 'filesystem_provenance_sha256':sha((p/'filesystem-provenance.json').read_bytes())})
base.pop('independent_review_sha256',None)
write_json('source-patch-manifest.json',base)
write_json('PREPARATION.json',{
 'status':'PREPARATION_ONLY_UNCOMPILED_UNFORMATTED_UNRUN','diagnostic_revision':'preparation-r3','event_schema':3,
 'base_source':base['base_commit'],'base_tree':base['base_tree'],'original_failed_evidence':base['sealed_original_evidence'],
 'original_failed_test_binary_sha256':base['original_failed_test_binary_sha256'],
 'original_failure':'exit 101 remains unattributed; no pass clears it; no storage-cause claim',
 'immutable_prior_preparations':immutable,'independent_reviews':reviews,
 'patch_sha256':patch_hash,'source_manifest_sha256':sha((p/'source-patch-manifest.json').read_bytes()),
 'test_only_paths':paths,'production_input_count':len(base['production_inputs']),'production_changes':[],
 'source_text_checks_only':['Model Debug derive in candidate and generator','append_log/replay/row_diff/step byte-identical bodies',
 'pure flags seam used; no after-pass snapshot branch or pre-oracle signal assertion'],
 'original_failure_configuration':{'profile':'release','TMPDIR':'/home/meszmate/.cache/a167/tmp','filesystem_at_failure':'NOT_CAPTURED'},
 'current_filesystem_metadata':provenance['current_preparation_cache_mount'],'metadata_source':'read-only /proc/self/mountinfo, not a storage probe or historical-medium proof',
 'performed':['Read FULL cached R2 independent review and R1/R2 artifacts',
 'Created cache-only R3 sourcecopies/patch/documents/manifests and retained exact sealed failure-plan bytes',
 'Read-only Git object identity and immutable-cache checksum checks; source-text inspection only',
 'Read kernel mount-table metadata to record configured/current filesystem provenance'],
 'not_performed':['Source/ref/index/target mutation','Patch application anywhere','Compilation/parser controls/tests/rustfmt',
 'Engine/crash/SSI/storage execution or probes/reruns','Services/containers/children','Push/export'],
 'future_limits':{'free_disk_floor_gib':30,'hard_capture_budget_gib':12,'minimum_initial_free_gib':42,'image_data_byte_cap_gib':2,
 'record_cap_mib':1,'dump_raw_cap_mib':16,'history_data_per_process_mib':128,'history_summary_per_process_mib':8,
 'failure_data_per_process_mib':256,'failure_summary_per_process_mib':8,'child_watchdogs':'300s entry and 5s post-arm; classified state outside image',
 'overall_deadline':'20-minute WHOLE PROCESS GROUP; TERM then KILL after 5 seconds; direct preverified release binary'},
 'limitations':['All R3 code remains UNCOMPILED/UNFORMATTED/UNRUN; source corrections are not a compiler verdict',
 'Instrumentation changes binary/schedule/I/O/crash boundary; no source or original-medium equivalence',
 'Seed alone cannot reconstruct retained original model/failure; seed+journal is not full interrupted state; preserve models/digests',
 'Budgets/caps/lost markers/missing dumps/timeouts/reset/query errors make evidence incomplete',
 'I/O failure can also defeat channel marker update; no infallible-storage promise',
 'Planned signal origin/time unproved even with armed SIGKILL and no recorded watchdog; pre-arm/watchdog exits rejected',
 'Failure snapshots are separate statements; actual chosen scan paths/timeouts not proven; SIGKILL is not power loss']})
files=sorted(f for f in p.rglob('*') if f.is_file() and f.name!='SHA256SUMS')
(p/'SHA256SUMS').write_text(''.join(sha(f.read_bytes())+'  '+str(f.relative_to(p))+'\n' for f in files))
print(json.dumps({'payloads':len(files),'patch_sha256':patch_hash,'source_manifest_sha256':sha((p/'source-patch-manifest.json').read_bytes()),
 'sha256sums_sha256':sha((p/'SHA256SUMS').read_bytes()),'prior_payloads_unchanged':[entry['payloads'] for entry in immutable],
 'candidate_execution':'NONE','runtime_authorized':False}))
