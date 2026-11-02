"""Experimental replay copy only: align mapping pgoff with ELF virtual addresses."""
from pathlib import Path
import struct,json,hashlib
root=Path(__file__).resolve().parent
p=next(root.glob('pilot-*/pilot.data'))
data=bytearray(p.read_bytes())
assert data[:8]==b'PERFILE2'
start,length=struct.unpack_from('<QQ',data,40)
offset=start;changes=[]
while offset<start+length:
 kind,misc,size=struct.unpack_from('<IHH',data,offset)
 assert size>=8
 if kind==10:
  name=bytes(data[offset+72:offset+size]).split(b'\0')[0].decode(errors='replace')
  if name.endswith('/histogram-bin/roll-engine'):
   old,=struct.unpack_from('<Q',data,offset+32)
   adjustment={0:0,0x1b52000:0x1000,0x4210000:0x2000,0x452e000:0x3000,0x4542000:0x3000}.get(old)
   assert adjustment is not None,hex(old)
   if adjustment:
    struct.pack_into('<Q',data,offset+32,old+adjustment)
    changes.append({'record_offset':offset,'original_pgoff':hex(old),'replay_pgoff':hex(old+adjustment)})
 offset+=size
assert offset==start+length
out=p.with_name('experimental-replay.data');out.write_bytes(data)
(out.parent/'experimental-replay.json').write_text(json.dumps({'warning':'Synthetic replay offsets; original perf.data unchanged. Decoded symbol names may be shifted. Validate raw addresses against original ELF/maps before attribution.','original_sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'replay_sha256':hashlib.sha256(data).hexdigest(),'changes':changes},indent=2)+'\n')
print(changes)
