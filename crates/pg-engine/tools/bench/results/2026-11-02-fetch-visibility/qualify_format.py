"""Compare formatter complaints in HEAD and current changed files without editing either."""
from pathlib import Path
import hashlib,json,re,subprocess,tempfile
REPO=Path('/home/meszmate/worktrees/roll/t3-d4020b98')
ROOT=Path(__file__).resolve().parent
results=[]
with tempfile.TemporaryDirectory(prefix='fetch-fmt-') as scratch:
    for name in ['crates/pg-engine/src/exec/sizing.rs','crates/pg-engine/tests/wire.rs']:
        current=(REPO/name).read_bytes()
        complaints=[]
        for label,data in [('baseline',subprocess.check_output(['git','show','HEAD:'+name],cwd=REPO)),('candidate',current)]:
            path=Path(scratch)/(label+'.rs');path.write_bytes(data)
            process=subprocess.run(['rustfmt','--edition','2024','--config','skip_children=true','--check',str(path)],capture_output=True,text=True)
            if process.returncode not in [0,1] or process.stderr:
                raise RuntimeError(process.stderr)
            complaints.append(re.sub(r'Diff in .*?:\d+:','Diff in SOURCE:',process.stdout))
        results.append({'file':name,'source_sha256':hashlib.sha256(current).hexdigest(),'baseline_and_candidate_complaints_equal_ignoring_shifted_line_numbers':complaints[0]==complaints[1]})
record={'workspace_fmt_exit':1,'qualification':'Existing baseline formatting differences remain; comparison ignores file paths and shifted line numbers only. No formatter edits made.','changed_files':results}
(ROOT/'format-qualification.json').write_text(json.dumps(record,indent=2)+'\n')
if not all(v['baseline_and_candidate_complaints_equal_ignoring_shifted_line_numbers'] for v in results):
    raise RuntimeError('New formatter complaints; inspect changed files')
print('Formatter complaints equal to HEAD on both changed files.')
