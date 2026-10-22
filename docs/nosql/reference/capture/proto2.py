import os, sys, json, struct
sys.argv = ['x']
exec(open('/cap/proto.py').read().split("S = lambda d: smap(d)")[0])
S = lambda d: smap(d)
res = {}
for v in (1, 2):
    res['OPTIONS v%d 9-byte header' % v] = probe(struct.pack('>BBhBI', v, 0, 1, 5, 0))
    res['OPTIONS v%d 8-byte header + 1 pad byte' % v] = probe(frame(v, 0, 5) + b'\x00')
    res['STARTUP v%d 8-byte header' % v] = probe(frame(v, 0, 1, S({'CQL_VERSION': '3.0.0'})))
res['first byte only 0x01'] = probe(b'\x01')
print(json.dumps(res, indent=1))
