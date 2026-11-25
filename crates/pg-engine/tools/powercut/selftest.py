#!/usr/bin/env python3
"""powercut.so and rebuild.py against known cases: a child python, under the layer, makes each
change the model covers; the cut must leave exactly what POSIX promises. Run alone (a second):

    crates/pg-engine/tools/powercut/selftest.py
"""
import os
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import rebuild  # noqa: E402

CHILD = r'''
import os, sys
r = sys.argv[1]
def w(name, data, flags=os.O_WRONLY | os.O_CREAT, sync=False):
    fd = os.open(os.path.join(r, name), flags, 0o644)
    os.write(fd, data)
    if sync: os.fsync(fd)
    os.close(fd)
def dsync(d=''):
    fd = os.open(os.path.join(r, d) if d else r, os.O_RDONLY)
    os.fsync(fd); os.close(fd)
w('new_synced', b'kept', sync=True); dsync()           # both: there
os.mkdir(os.path.join(r, 'sub2')); dsync(); w('sub2/g', b'g', sync=True); dsync('sub2')  # there
# Each change below is followed by no fsync of the root.
w('new_unsynced_dir', b'x', sync=True)                 # file fsynced, its directory not: gone
fd = os.open(os.path.join(r, 'old'), os.O_WRONLY); os.pwrite(fd, b'ZZ', 1); os.close(fd)  # not synced: old bytes
w('tmp', b'new manifest', flags=os.O_WRONLY | os.O_CREAT | os.O_TRUNC, sync=True)
os.rename(os.path.join(r, 'tmp'), os.path.join(r, 'manifest'))   # no directory fsync: the old one
os.unlink(os.path.join(r, 'gone'))                     # not synced: back
os.mkdir(os.path.join(r, 'sub')); w('sub/f', b's', sync=True); dsync('sub')  # parent not synced: gone
fd = os.open(os.path.join(r, 'log'), os.O_WRONLY | os.O_APPEND)
os.write(fd, b'+synced'); os.fdatasync(fd); os.write(fd, b'+lost'); os.close(fd)
w('trunc', b'', flags=os.O_WRONLY | os.O_TRUNC)        # O_TRUNC unsynced: content back
fd = os.open(os.path.join(r, 'big'), os.O_RDWR); os.ftruncate(fd, 3); os.close(fd)   # size change: undone
os.rename(os.path.join(r, 'mv'), os.path.join(r, 'sub2', 'mv')); dsync('sub2')       # src dir unsynced: in both
'''


def main():
    with tempfile.TemporaryDirectory(prefix='a325-powercut-') as t:
        lib = rebuild.build_lib(t)
        root = os.path.join(t, 'root')
        os.mkdir(root)
        for name, data in (('old', b'abcdef'), ('manifest', b'old manifest'), ('gone', b'g'), ('log', b'L'),
                           ('trunc', b'truncated?'), ('big', b'0123456789'), ('mv', b'm')):
            with open(os.path.join(root, name), 'wb') as f:
                f.write(data)
        pc = rebuild.PowerCut(root, os.path.join(t, 'state'), lib)
        os.makedirs(pc.state)
        env = dict(os.environ, **pc.env())
        subprocess.check_call([sys.executable, '-I', '-c', CHILD, root], env=env)
        stats = pc.cut('drop', 0)
        got = {}
        for d, _, fs in os.walk(root):
            for f in fs:
                p = os.path.join(d, f)
                got[os.path.relpath(p, root)] = open(p, 'rb').read()
        want = {'new_synced': b'kept', 'old': b'abcdef', 'manifest': b'old manifest', 'gone': b'g',
                'log': b'L+synced', 'trunc': b'truncated?', 'big': b'0123456789', 'sub2/g': b'g',
                'mv': b'm', 'sub2/mv': b'm'}
        bad = [f'{k}: {got.get(k)!r}, want {want.get(k)!r}' for k in sorted(set(got) | set(want)) if got.get(k) != want.get(k)]
        print('powercut selftest:', 'ok' if not bad else 'FAILED', stats)
        for b in bad:
            print('  ' + b)
        sys.exit(1 if bad else 0)


if __name__ == '__main__':
    main()
