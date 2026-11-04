import hashlib,json,subprocess
from pathlib import Path
scratch=Path('/tmp/combined-index-models-d62e3d3b/proof-r1-successor')
old='4e588363d065aa9f12dc1b987d22dbf6742dcb9f'; final='ca45637beccaf8d4183ae1eb2bafc7227b9dd355'
b4='b4dfee71aafd1c291ebccaf5e44173ada3474029'; red=(scratch/'red-identity.txt').read_text().splitlines()[0]
base='fe7b0023c9df19a6dae1e6067ba8454998db48c5'
allowed=['crates/pg-engine/src/exec/rowsize/observe.rs','crates/pg-engine/src/exec/sizing/kept.rs','crates/pg-engine/src/session/ddl/parbuild.rs']
def git(*args): return subprocess.check_output(['git',*args])
def blob(ref,p): return git('show',ref+':'+p)
def sha(data): return hashlib.sha256(data).hexdigest()
def project(p,data):
 if p.endswith('/observe.rs'): return b''
 if p.endswith('/kept.rs'): return data[:data.index(b'#[cfg(test)]\nmod tests {')]
 if p.endswith('/parbuild.rs'):
  start=data.index(b'            #[cfg(test)]\n            if first {'); brace=data.index(b'{',start); depth=0
  for i in range(brace,len(data)):
   depth+=(data[i]==123)-(data[i]==125)
   if depth==0: return data[:start]+data[i+1:]
  raise ValueError('unterminated first-chunk test hook')
 raise ValueError(p)
reports=[]
for label,before,after in [('candidate',old,final),('red',b4,red)]:
 changed=git('diff','--name-only',before,after).decode().splitlines();assert sorted(changed)==sorted(allowed),changed
 rows=[]
 for path in changed:
  original=blob(before,path);successor=blob(after,path);a=project(path,original);b=project(path,successor);assert a==b,(label,path)
  rows.append({'path':path,'before_blob':git('rev-parse',before+':'+path).decode().strip(),'after_blob':git('rev-parse',after+':'+path).decode().strip(),'production_projection_before_sha256':sha(a),'production_projection_after_sha256':sha(b),'production_bytes_equal':True})
 # The excluded observer file is attached through an unchanged cfg(test) declaration.
 attach='crates/pg-engine/src/exec/rowsize.rs';assert blob(before,attach)==blob(after,attach)
 assert b'#[cfg(test)]\npub(crate) mod observe;' in blob(after,attach)
 reports.append({'label':label,'before':before,'after':after,'changed_source_paths':rows,'all_other_paths_have_identical_blobs':True,'projection':'Exclude the wholly cfg(test) observer module, existing kept::tests tail, and outer cfg(test) if-first block only. Remaining bytes compare exactly.'})
# The old modeled row implementation stays byte-for-byte unchanged in the red reference.
p='crates/pg-engine/src/session/ddl/parbuild.rs'
def row(ref):
 d=blob(ref,'crates/pg-engine/src/session/ddl/parbuild.rs');a=d.index(b'    fn row(');b=d.index(b'    /// Take chunks',a);return d[a:b]
assert row(b4)==row(red)
(scratch/'production-preservation.json').write_text(json.dumps({'status':'Static byte verification only; never compiled or run','checks':reports,'red_Plan_row_matches_b4_sha256':sha(row(red))},indent=2)+'\n')
for label,ref,branch in [('candidate',final,'feat/combined-index-validation'),('red',red,'wip/combined-index-models-red-proof-r1-d62e3d3b')]:
 files=[]
 for p in git('diff','--name-only',base,ref).decode().splitlines():
  data=blob(ref,p)
  files.append({'path':p,'blob':git('rev-parse',ref+':'+p).decode().strip(),'sha256':sha(data),'bytes':len(data)})
 manifest={'status':'UNTESTED WIP; no source or performance acceptance','head':ref,'tree':git('rev-parse',ref+'^{tree}').decode().strip(),'branch':branch,'base':base,'files':files}
 (scratch/(label+'-manifest.json')).write_text(json.dumps(manifest,indent=2)+'\n')
 (scratch/(label+'-scoped.diff')).write_bytes(git('diff','--binary',old if label=='candidate' else b4,ref))
print('Candidate and red changes limited to three test/observer paths; production bytes preserved against their respective parents.')
print('Red Plan::row exact-match SHA256:',sha(row(red)))
print('Candidate tree:',git('rev-parse',final+'^{tree}').decode().strip())
print('Red tree:',git('rev-parse',red+'^{tree}').decode().strip())
