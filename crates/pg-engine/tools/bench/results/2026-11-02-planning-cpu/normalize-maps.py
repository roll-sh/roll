"""Create an offline-only perf replay with PT_LOAD virtual mapping offsets.

Original data is never overwritten. Replay symbol labels are invalid because perf
applies another ELF offset adjustment; consume raw IPs and symbolize against the
original mappings and ELF. This works around perf 6.15 libdw's mapping bias bug.
"""
from pathlib import Path
import hashlib
import json
import struct
import sys

source, binary, output = map(Path, sys.argv[1:])
assert source.resolve() != output.resolve() and not output.exists()
elf = binary.read_bytes()
assert elf[:6] == b'\x7fELF\x02\x01'
phoff = struct.unpack_from('<Q', elf, 32)[0]
phsize, count = struct.unpack_from('<HH', elf, 54)
loads = []
for index in range(count):
    kind, flags, offset, virtual, physical, size, memsize, align = struct.unpack_from('<IIQQQQQQ', elf, phoff + index * phsize)
    if kind == 1:
        loads.append((offset & ~4095, virtual & ~4095, (offset + size + 4095) & ~4095, bool(flags & 1)))
data = bytearray(source.read_bytes())
assert data[:8] == b'PERFILE2'
start, size = struct.unpack_from('<QQ', data, 40)
pos = start
changes = []
while pos < start + size:
    kind, misc, length = struct.unpack_from('<IHH', data, pos)
    assert length >= 8 and pos + length <= start + size
    if kind == 10:
        name = bytes(data[pos + 72:pos + length]).split(b'\0')[0].decode()
        if name == str(binary):
            old = struct.unpack_from('<Q', data, pos + 32)[0]
            execute = bool(struct.unpack_from('<I', data, pos + 64)[0] & 4)
            matches = [(offset, virtual) for offset, virtual, end, executable in loads if offset <= old < end and executable == execute]
            if len(matches) > 1:
                matches = [max(matches)]
            assert len(matches) == 1, (hex(old), matches)
            offset, virtual = matches[0]
            new = old + virtual - offset
            if new != old:
                struct.pack_into('<Q', data, pos + 32, new)
                changes.append({'record_offset': pos, 'old_pgoff': old, 'new_pgoff': new})
    pos += length
assert pos == start + size and changes
with output.open('xb') as f:
    f.write(data)
output.with_suffix('.json').write_text(json.dumps({
    'synthetic_replay_only': True,
    'original': str(source), 'original_sha256': hashlib.sha256(source.read_bytes()).hexdigest(),
    'binary': str(binary), 'binary_sha256': hashlib.sha256(elf).hexdigest(),
    'replay_sha256': hashlib.sha256(data).hexdigest(), 'changes': changes,
    'warning': 'Consume raw IPs only. Replay symbol labels are shifted. Resolve IPs through original mappings/ELF.'
}, indent=2) + '\n')
