"""Symbolize synthetic-replay raw addresses against ORIGINAL mappings and ELF."""
from pathlib import Path
import subprocess,struct,re,bisect,json
from collections import Counter
root=Path(__file__).resolve().parent
run=next(root.glob('pilot-*/pilot.data')).parent
binary=Path('/home/meszmate/.cache/pg-engine-perf-20261101/oltp-20261102/histogram-bin/roll-engine')
buf=(run/'pilot.data').read_bytes(); start,size=struct.unpack_from('<QQ',buf,40)
pos=start;bias=None
while pos<start+size:
 kind,misc,length=struct.unpack_from('<IHH',buf,pos)
 if kind==10 and bytes(buf[pos+72:pos+length]).split(b'\0')[0].endswith(b'/histogram-bin/roll-engine'):
  addr,span,off=struct.unpack_from('<QQQ',buf,pos+16)
  if off==0:bias=addr
 pos+=length
assert bias is not None
symbols=[]
for line in subprocess.check_output(['nm','-n','-S','-C',str(binary)],text=True).splitlines():
 m=re.match(r'^([0-9a-f]+) ([0-9a-f]+) ([tT]) (.*)$',line)
 if m:symbols.append((int(m[1],16),int(m[2],16),m[4]))
starts=[x[0] for x in symbols]
def symbolize(ip):
 address=int(ip,16)-bias;i=bisect.bisect_right(starts,address)-1
 if i>=0:
  start,length,name=symbols[i]
  if start<=address<start+length:return {'ip':ip,'elf_address':hex(address),'symbol':name,'offset':hex(address-start)}
 return {'ip':ip,'symbol':None}
original=(run/'stacks.txt').read_text().strip().split('\n\n')
replay=(run/'experimental-replay-stacks.txt').read_text().strip().split('\n\n')
assert len(original)==len(replay)==79
samples=[]
for a,b in zip(original,replay):
 assert a.splitlines()[0]==b.splitlines()[0]
 raw=[line.strip().split()[0] for line in b.splitlines()[1:] if line.startswith('\t')]
 assert raw[0]==a.splitlines()[1].strip().split()[0]
 frames=[symbolize(x) for x in raw]
 names=[x['symbol'] or '' for x in frames]
 if any('<pg_engine::session::Session>::bound_custom' in x for x in names):bucket='custom_binding'
 elif any('pg_engine::session::plan_cost_estimate' in x or '<pg_engine::session::Session>::plan_cost'==x for x in names):bucket='root_costing'
 elif any('<pg_engine::session::Session>::planned_as'==x for x in names):bucket='other_planning'
 else:bucket='unattributed'
 samples.append({'header':b.splitlines()[0],'bucket':bucket,'frames':frames})
summary={'warning':'Pilot only, 79 samples. Synthetic mmap replay used solely to recover raw IPs; symbol names use the original mapping and original ELF. Unattributed samples are not redistributed. Periods are uniform. No performance or CPU phase-share conclusion.','original_load_bias':hex(bias),'samples':len(samples),'buckets':dict(Counter(s['bucket'] for s in samples)),'replay_depths':dict(sorted(Counter(len(s['frames']) for s in samples).items()))}
(run/'classified.json').write_text(json.dumps({'summary':summary,'samples':samples},indent=2)+'\n')
print(json.dumps(summary,indent=2))
for bucket in ['root_costing','custom_binding']:
 s=next((s for s in samples if s['bucket']==bucket),None)
 if s:
  print(bucket)
  for f in s['frames']:
   if f['symbol']:print(f['elf_address'],f['symbol'],f['offset'])
