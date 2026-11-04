import os,subprocess
from pathlib import Path
scratch=Path('/tmp/combined-index-models-d62e3d3b/proof-r1-successor')
def git(*args,env=None,data=None):
 return subprocess.check_output(['git',*args],env=env,input=data)
base='b4dfee71aafd1c291ebccaf5e44173ada3474029'
final='ca45637beccaf8d4183ae1eb2bafc7227b9dd355'
patch=git('diff','--binary','4e588363d065aa9f12dc1b987d22dbf6742dcb9f',final)
(scratch/'proof-corrections.patch').write_bytes(patch)
env=os.environ.copy();env['GIT_INDEX_FILE']=str(scratch/'red.index')
git('read-tree',base,env=env)
git('apply','--cached','--check',str(scratch/'proof-corrections.patch'),env=env)
git('apply','--cached',str(scratch/'proof-corrections.patch'),env=env)
git('diff','--cached','--check',base,env=env)
tree=git('write-tree',env=env).decode().strip()
head=git('commit-tree',tree,'-p',base,data=b'WIP: correct proof tests on b4 red reference; modeled early-return remains; never run\n').decode().strip()
ref='refs/heads/wip/combined-index-models-red-proof-r1-d62e3d3b'
git('update-ref',ref,head,'0'*40)
(scratch/'red-identity.txt').write_text(head+'\n'+tree+'\n'+ref+'\n')
print(head,tree,ref)
print(git('diff','--stat',base,head).decode())
