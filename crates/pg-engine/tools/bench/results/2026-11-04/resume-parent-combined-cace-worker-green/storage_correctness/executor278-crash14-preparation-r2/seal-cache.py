# Read Git objects + cache bytes and seal preparation artifacts. No candidate execution.
from pathlib import Path
import difflib, hashlib, json, subprocess
p=Path(__file__).parent
r1=p.parent/'executor278-crash14-preparation-r1'
def sha(data): return hashlib.sha256(data).hexdigest()
def write_json(name, value): (p/name).write_text(json.dumps(value,indent=2)+'\n')
# Validate immutable r1 payloads through its existing checksum list; never rewrite it.
r1_entries=[]
for line in (r1/'SHA256SUMS').read_text().splitlines():
 expected, name=line.split('  ',1)
 actual=sha((r1/name).read_bytes())
 if actual != expected: raise RuntimeError('r1 hash mismatch: '+name)
 r1_entries.append({'path':name,'sha256':actual})
# Read-only exact object verification. cat-file never writes index, refs or checkout.
base=json.loads((r1/'source-patch-manifest.json').read_text())
proc=subprocess.Popen(['git','cat-file','--batch'],stdin=subprocess.PIPE,stdout=subprocess.PIPE)
def blob(oid):
 proc.stdin.write((oid+'\n').encode()); proc.stdin.flush()
 header=proc.stdout.readline().decode().strip().split()
 if len(header)!=3 or header[1] not in ('blob','commit'): raise RuntimeError(header)
 size=int(header[2]); data=proc.stdout.read(size)
 if proc.stdout.read(1)!=b'\n': raise RuntimeError('bad framing')
 return data
commit=blob(base['base_commit']).decode()
if not commit.startswith('tree '+base['base_tree']+'\n'): raise RuntimeError('base tree mismatch')
original=(p/'original/crates/pg-engine/tests/crash_random.rs').read_bytes()
if blob(base['base_harness_git_blob'])!=original: raise RuntimeError('original blob mismatch')
for entry in base['production_inputs']:
 data=blob(entry['git_blob'])
 if len(data)!=entry['bytes'] or sha(data)!=entry['sha256']: raise RuntimeError('input object mismatch: '+entry['path'])
proc.stdin.close(); proc.wait()
if proc.returncode: raise RuntimeError('cat-file failed')
paths=['crates/pg-engine/tests/crash_random.rs','crates/pg-engine/tests/support/crash14_diagnostic.rs']
changes=[]; patch=[]
for name in paths:
 before=(p/'original'/name).read_bytes() if (p/'original'/name).exists() else b''
 after=(p/'candidate'/name).read_bytes()
 changes.append({'path':name,'before_sha256':sha(before) if before else None,
                 'after_sha256':sha(after),'before_bytes':len(before),'after_bytes':len(after)})
 patch.append('diff --git a/'+name+' b/'+name+'\n')
 if not before: patch.append('new file mode 100644\n')
 patch.extend(difflib.unified_diff(before.decode().splitlines(True),after.decode().splitlines(True),
                  fromfile='a/'+name if before else '/dev/null',tofile='b/'+name))
(p/'diagnostic-test-only.patch').write_text(''.join(patch))
patch_hash=sha((p/'diagnostic-test-only.patch').read_bytes())
base.update({'revision':'preparation-r2','patch_sha256':patch_hash,'test_only_changes':changes,
             'independent_review_sha256':sha((p/'independent-review-r1.json').read_bytes()),
             'input_verification':'Exact base commit/tree, harness blob and all 882 listed production/build-input Git blob bytes checked via read-only cat-file; no future workspace/compiler claim.',
             'parent_r1_manifest_sha256':sha((r1/'source-patch-manifest.json').read_bytes())})
write_json('source-patch-manifest.json',base)
write_json('PREPARATION.json',{
 'status':'PREPARATION_ONLY_UNCOMPILED_UNFORMATTED_UNRUN',
 'revision':'r2','base_source':base['base_commit'],'base_tree':base['base_tree'],
 'original_failed_evidence':base['sealed_original_evidence'],
 'original_failed_test_binary_sha256':base['original_failed_test_binary_sha256'],
 'original_failure':'exit 101 remains unattributed; no pass clears it; no storage-cause claim',
 'original_r1_immutable':str(r1),'r1_payload_checksums_unchanged':True,
 'r1_sha256sums_sha256':sha((r1/'SHA256SUMS').read_bytes()),
 'review_task_id':json.loads((p/'independent-review-r1.json').read_text())['taskId'],
 'review_result_sha256':sha((p/'independent-review-r1.json').read_bytes()),
 'patch_sha256':patch_hash,'source_manifest_sha256':sha((p/'source-patch-manifest.json').read_bytes()),
 'test_only_paths':paths,'production_input_count':len(base['production_inputs']),'production_changes':[],
 'performed':['Read full cached independent r1 review and r1 sourcecopies/documents',
              'Created r2 sibling cache-only sourcecopies, two-file patch, documents and manifests',
              'Read-only exact Git commit/tree and blob-byte identity checks for original harness and 882 listed inputs',
              'Textual inspection and cache-artifact SHA256 checks only; no candidate/parser execution'],
 'not_performed':['Source checkout/index/ref/target writes','Patch application to any source tree',
                  'Builds/tests/parser controls/rustfmt','Services/containers/crash/SSI/storage probes/reruns',
                  'Child agents','Push/export'],
 'future_limits':{'capture_root':'NEW persistent disk cache, never tmpfs',
                  'free_disk_floor_gib':30,'hard_capture_budget_gib':12,'minimum_initial_free_gib':42,
                  'image_data_byte_cap_gib':2,'event_channel_cap_mib':256,
                  'overall_deadline':'20-minute whole process group; TERM then KILL after 5 seconds',
                  'child_watchdogs':'300 seconds from entry; 5 seconds after arm; diagnostic only'},
 'limitations':['All r2 candidate code remains uncompiled/unformatted/unrun',
                'r1 review found no known compile blockers in r1 only',
                'Instrumentation changes binary, timing and crash boundary',
                'Seed alone cannot reproduce retained original; seed+journal is not all interrupted state',
                'Full models retained at boundaries; SQL digest/length events do not prove exact killed instruction',
                'After-check/failure SQL consists of separate snapshots; timeouts/errors/caps mean incomplete evidence',
                'SIGKILL is not power loss; planned versus watchdog SIGKILL not distinguished']})
# Closed membership, excluding only the checksum file itself.
files=sorted(f for f in p.rglob('*') if f.is_file() and f.name!='SHA256SUMS')
(p/'SHA256SUMS').write_text(''.join(sha(f.read_bytes())+'  '+str(f.relative_to(p))+'\n' for f in files))
print(json.dumps({'payloads':len(files),'patch_sha256':patch_hash,
 'manifest_sha256':sha((p/'source-patch-manifest.json').read_bytes()),
 'sha256sums_sha256':sha((p/'SHA256SUMS').read_bytes()),'r1_payloads_unchanged':len(r1_entries),
 'execution':'NONE: only artifact construction, read-only Git object identity and checksums'}))
