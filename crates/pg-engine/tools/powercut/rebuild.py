#!/usr/bin/env python3
"""Rebuild a directory as a power cut would have left it, from powercut.so's record.

    tools/powercut/rebuild.py <state dir> [--mode drop|keep] [--seed N]

The state dir is what `PowerCut` (below) set up for one run of a process: `power-on.json` (the
tree when the process started: everything in it counts as durable), `log` (powercut.c's
records) and `grave/` (entries the process removed or replaced, kept for the cut). The watched
directory is given in power-on.json.

The model, per POSIX rather than per any one filesystem (ext4's journal makes more durable than
this; a filesystem without one, or another kernel, may not):
  - a file's bytes and size are what they were at its last fsync / fdatasync that returned
    (or sync(2)); writes and size changes since are gone;
  - a directory's entries (files created, renamed, linked, removed in it) are what they were at
    its last fsync (or sync(2)); a file whose entry was never fsynced into its directory is
    gone, a removal or rename never fsynced is undone (the entry comes back, from the grave);
  - `keep` (a disk that wrote part of its cache before the power went): per file, a seeded
    prefix of its unsynced writes stays, and the next one may be torn (a seeded subset of its
    4 KiB pages); and a seeded prefix of all the unsynced name changes stays, in their order
    (a journal).
`drop` (the default) keeps nothing that was not made durable: the strictest case, exact.

Before cutting, the record is checked against the real directory: every entry and every file's
size must be what the record says (one entry may differ: the change the process was making
when it was killed). A mismatch, or an operation powercut.so can't model, is the tool's
failure, not the program's: `ToolError`.
"""
import argparse
import json
import os
import random
import shutil
import struct
import sys

HDR = struct.Struct('<IB3xIIQQQQQ')
PAGE = 4096


class ToolError(Exception):
    pass


def snapshot(root):
    """The tree under root: {'root', 'dirs': {ino: {name: ino}}, 'kind': {ino: 'f'|'d'}, 'size': {ino: n}}."""
    st = os.lstat(root)
    out = {'path': root, 'root': st.st_ino, 'dirs': {}, 'kind': {st.st_ino: 'd'}, 'size': {}}

    def walk(path, ino):
        ents = {}
        for e in os.scandir(path):
            s = e.stat(follow_symlinks=False)
            ents[e.name] = s.st_ino
            if e.is_dir(follow_symlinks=False):
                out['kind'][s.st_ino] = 'd'
                walk(e.path, s.st_ino)
            elif e.is_file(follow_symlinks=False):
                out['kind'][s.st_ino] = 'f'
                out['size'][s.st_ino] = s.st_size
            else:
                raise ToolError(f'{e.path}: neither a file nor a directory')
        out['dirs'][ino] = ents

    walk(root, st.st_ino)
    return out


def save_snapshot(snap, path):
    j = {'path': snap['path'], 'root': snap['root'],
         'dirs': {str(k): v for k, v in snap['dirs'].items()},
         'kind': {str(k): v for k, v in snap['kind'].items()},
         'size': {str(k): v for k, v in snap['size'].items()}}
    with open(path, 'w') as f:
        json.dump(j, f)


def load_snapshot(path):
    with open(path) as f:
        j = json.load(f)
    return {'path': j['path'], 'root': j['root'],
            'dirs': {int(k): dict(v) for k, v in j['dirs'].items()},
            'kind': {int(k): v for k, v in j['kind'].items()},
            'size': {int(k): v for k, v in j['size'].items()}}


def records(path):
    with open(path, 'rb') as f:
        data = f.read()
    p, out = 0, []
    while p + HDR.size <= len(data):
        size, kind, n1, n2, seq, a, b, c, d = HDR.unpack_from(data, p)
        if p + size > len(data):
            break  # (not possible: each record is one write)
        q = p + HDR.size
        name1 = data[q:q + n1].decode(errors='surrogateescape')
        name2 = data[q + n1:q + n1 + n2].decode(errors='surrogateescape')
        payload = data[q + n1 + n2:p + size]
        out.append((chr(kind), seq, a, b, c, d, name1, name2, payload))
        p += size
    return out


class Replay:
    """The record replayed over the power-on tree."""

    def __init__(self, snap, recs):
        self.snap = snap
        self.dirs = {k: dict(v) for k, v in snap['dirs'].items()}
        self.kind = dict(snap['kind'])
        self.size = dict(snap['size'])
        self.created = set()  # directories made during the run (no power-on entries)
        self.dir_ops = {}  # dir ino -> [(seq, name, ino or None)]
        self.file_recs = {}  # file ino -> [(seq, kind, a.., payload)]
        self.fcover, self.dcover = {}, {}
        self.gcover = 0
        self.grave = {}  # ino -> grave file name (a place its bytes still are)
        self.meta = []  # seqs of name changes, in order
        self.issues = []
        self.counts = {}
        for r in recs:
            self.apply(r)

    def entry(self, seq, dino, name, ino):
        if dino not in self.dirs:
            self.issues.append(f'record {seq}: directory inode {dino} unknown')
            self.dirs[dino] = {}
        if ino is None:
            self.dirs[dino].pop(name, None)
        else:
            self.dirs[dino][name] = ino
        self.dir_ops.setdefault(dino, []).append((seq, name, ino))

    def apply(self, r):
        k, seq, a, b, c, d, n1, n2, payload = r
        self.counts[k] = self.counts.get(k, 0) + 1
        if k == 'W':
            if self.size.get(a) != d:
                self.issues.append(f'record {seq}: a write to inode {a} found size {d}, the record says {self.size.get(a)}')
            self.size[a] = max(d, b + c)
            self.file_recs.setdefault(a, []).append(r)
        elif k == 'T':
            if self.size.get(a) != d:
                self.issues.append(f'record {seq}: a size change of inode {a} found size {d}, the record says {self.size.get(a)}')
            self.size[a] = b
            self.file_recs.setdefault(a, []).append(r)
        elif k == 'F':
            self.fcover[a] = max(self.fcover.get(a, 0), b)
        elif k == 'D':
            self.dcover[a] = max(self.dcover.get(a, 0), b)
            # A directory's fsync makes its own data durable too: nothing else to do.
        elif k == 'S':
            self.gcover = max(self.gcover, b)
        elif k == 'C':
            self.kind[a] = 'f'
            self.size[a] = 0
            self.entry(seq, b, n1, a)
            self.meta.append(seq)
        elif k == 'M':
            self.kind[a] = 'd'
            self.dirs[a] = {}
            self.created.add(a)
            self.entry(seq, b, n1, a)
            self.meta.append(seq)
        elif k == 'R':
            self.entry(seq, b, n1, None)
            self.entry(seq, c, n2, a)
            if d:
                self.grave[d] = f'o{d}.{seq}'
            self.meta.append(seq)
        elif k == 'U':
            self.entry(seq, b, n1, None)
            self.grave[a] = f'g{a}.{seq}'
            self.meta.append(seq)
        elif k == 'L':
            self.entry(seq, b, n1, a)
            self.meta.append(seq)
        elif k == 'X':
            self.issues.append(f'record {seq}: powercut.so cannot model {n1} ({n2})')
        else:
            self.issues.append(f'record {seq}: unknown kind {k!r}')

    def entries_at(self, dino, t):
        """dino's entries with the name changes before seq t applied."""
        ents = {} if dino in self.created else dict(self.snap['dirs'].get(dino, {}))
        for seq, name, ino in self.dir_ops.get(dino, ()):
            if seq >= t:
                break
            if ino is None:
                ents.pop(name, None)
            else:
                ents[name] = ino
        return ents

    def where(self, root, grave):
        """Each file inode's current path: under root (the replayed tree) or in the grave."""
        loc = {}

        def walk(path, dino, depth=0):
            for name, ino in self.dirs.get(dino, {}).items():
                p = os.path.join(path, name)
                if self.kind.get(ino) == 'd':
                    if depth < 64:
                        walk(p, ino, depth + 1)
                else:
                    loc.setdefault(ino, p)
        walk(root, self.snap['root'])
        for ino, g in self.grave.items():
            loc.setdefault(ino, os.path.join(grave, g))
        return loc


def check_real(rep, root):
    """The replayed tree against the real one: (differences in entries, in sizes)."""
    diffs = []
    real = snapshot(root)
    if real['root'] != rep.snap['root']:
        return [f'the root is inode {real["root"]}, it was {rep.snap["root"]}']

    def walk(dino, path):
        want = rep.dirs.get(dino, {})
        have = real['dirs'].get(dino, {})
        for name in sorted(set(want) | set(have)):
            w, h = want.get(name), have.get(name)
            p = os.path.join(path, name)
            if w != h:
                diffs.append(f'{p}: the record has inode {w}, the directory {h}')
                continue
            if real['kind'][h] == 'd':
                walk(h, p)
            elif rep.size.get(h) != real['size'][h]:
                diffs.append(f'{p}: {real["size"][h]} bytes, the record says {rep.size.get(h)}')
    walk(rep.snap['root'], root)
    return diffs


def undo(buf, r):
    k, seq, a, b, c, d, n1, n2, payload = r
    if k == 'W':  # off b, len c, size before d, payload the bytes overwritten
        if payload:
            buf[b:b + len(payload)] = payload
        if len(buf) > d:
            del buf[d:]
        elif len(buf) < d:
            buf.extend(bytes(d - len(buf)))
    else:  # T: size b after, d before, payload the bytes cut off
        if b < d:
            del buf[b:]
            buf.extend(payload)
        else:
            del buf[d:]


def content(rep, ino, path, mode, rng, stats):
    """The file's bytes after the cut (None: it is unchanged since it was durable)."""
    t = max(rep.fcover.get(ino, 0), rep.gcover)
    pending = [r for r in rep.file_recs.get(ino, ()) if r[1] >= t]
    if not pending:
        return None
    with open(path, 'rb') as f:
        cur = bytearray(f.read())
    before = len(cur)
    n = len(pending)
    k = 0 if mode == 'drop' else rng.randint(0, n)
    tear = mode == 'keep' and k < n and rng.random() < 0.5
    buf = cur
    for r in reversed(pending[k + 1 if tear else k:]):
        undo(buf, r)
    if tear:
        nxt = bytes(buf)
        undo(buf, pending[k])
        off, ln = pending[k][3], pending[k][4]
        if pending[k][0] == 'W' and ln:
            first, last = off // PAGE, (off + ln - 1) // PAGE
            for p in range(first, last + 1):
                if rng.random() < 0.5:
                    lo, hi = max(p * PAGE, off), min((p + 1) * PAGE, off + ln, len(nxt))
                    if lo >= hi:
                        continue
                    if len(buf) < hi:
                        buf.extend(bytes(hi - len(buf)))
                    buf[lo:hi] = nxt[lo:hi]
        stats['torn'] += 1
    stats['files_cut'] += k < n
    stats['bytes_dropped'] += max(0, before - len(buf))
    stats['writes_dropped'] += n - k
    return bytes(buf)


def cut(state, mode='drop', seed=0, keep_old=None):
    """Replace the watched directory with what a power cut leaves; returns a summary dict."""
    snap = load_snapshot(os.path.join(state, 'power-on.json'))
    root = snap['path']
    grave = os.path.join(state, 'grave')
    recs = records(os.path.join(state, 'log')) if os.path.exists(os.path.join(state, 'log')) else []
    # (A rename or removal that failed after its record: N names it.)
    failed = {r[2] for r in recs if r[0] == 'N'}
    recs = [r for r in recs if r[0] != 'N' and r[1] not in failed]
    rep = Replay(snap, recs)
    if rep.issues:
        raise ToolError('the record is not consistent:\n  ' + '\n  '.join(rep.issues[:20]))
    diffs = check_real(rep, root)
    # A rename or removal is recorded before it is made: the kill may have come between the
    # two, so the last one may not have happened (the directory says).
    if diffs and recs and recs[-1][0] in 'RU':
        undone = Replay(snap, recs[:-1])
        left = check_real(undone, root)
        if len(left) < len(diffs):
            recs, rep, diffs = recs[:-1], undone, left
    if len(diffs) > 1:
        raise ToolError('the record does not match the directory (an operation powercut.so missed?):\n  ' + '\n  '.join(diffs[:20]))
    rng = random.Random(seed)
    kmeta = 0
    if mode == 'keep' and rep.meta:
        i = rng.randint(0, len(rep.meta))
        kmeta = rep.meta[i - 1] + 1 if i else 0
    loc = rep.where(root, grave)
    stats = {'records': len(recs), 'files_cut': 0, 'writes_dropped': 0, 'bytes_dropped': 0, 'torn': 0,
             'entries_dropped': 0, 'entries_back': 0, 'unmatched': diffs, 'cut_files': []}
    out = root.rstrip('/') + '.cut'
    shutil.rmtree(out, ignore_errors=True)
    done = {}

    def build(dino, path, depth=0):
        os.mkdir(path)
        t = max(rep.dcover.get(dino, 0), rep.gcover, kmeta)
        ents = rep.entries_at(dino, t)
        now = rep.dirs.get(dino, {})
        stats['entries_dropped'] += sum(1 for n, i in now.items() if ents.get(n) != i)
        stats['entries_back'] += sum(1 for n, i in ents.items() if now.get(n) != i)
        for name, ino in sorted(ents.items()):
            p = os.path.join(path, name)
            if rep.kind.get(ino) == 'd':
                if depth > 64:
                    raise ToolError(f'{p}: directories nested past 64')
                build(ino, p, depth + 1)
                continue
            src = loc.get(ino)
            if src is None or not os.path.exists(src):
                raise ToolError(f'{p}: inode {ino} is nowhere (not under the root, not in the grave)')
            if ino not in done:
                done[ino] = content(rep, ino, src, mode, rng, stats)
                if done[ino] is not None and done[ino] != open(src, 'rb').read():
                    stats['cut_files'].append(os.path.relpath(p, out))
            data = done[ino]
            if data is None:
                os.link(src, p)
            else:
                with open(p, 'wb') as f:
                    f.write(data)
    build(snap['root'], out)
    if keep_old:
        shutil.rmtree(keep_old, ignore_errors=True)
        os.rename(root, keep_old)
    else:
        shutil.rmtree(root)
    os.rename(out, root)
    shutil.rmtree(grave, ignore_errors=True)
    return stats


class PowerCut:
    """One watched run: set up the state dir, the environment for the process, and the cut."""

    def __init__(self, root, state, lib, exe):
        self.root, self.state, self.lib, self.exe = os.path.realpath(root), state, lib, os.path.realpath(exe)

    def env(self):
        """Snapshot the tree (its power-on state) and give the process's environment."""
        shutil.rmtree(os.path.join(self.state, 'grave'), ignore_errors=True)
        os.makedirs(os.path.join(self.state, 'grave'))
        try:
            os.remove(os.path.join(self.state, 'log'))
        except FileNotFoundError:
            pass
        save_snapshot(snapshot(self.root), os.path.join(self.state, 'power-on.json'))
        if os.stat(self.state).st_dev != os.stat(self.root).st_dev:
            raise ToolError(f'{self.state} and {self.root} are on different filesystems')
        return {'LD_PRELOAD': self.lib, 'POWERCUT_EXE': self.exe,
                'POWERCUT': f'{self.root}:{os.path.join(self.state, "grave")}:{os.path.join(self.state, "log")}'}

    def cut(self, mode, seed, keep_old=None):
        return cut(self.state, mode, seed, keep_old)


def build_lib(out_dir):
    """Compile powercut.so into out_dir (cc, a second or two); its path."""
    src = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'powercut.c')
    lib = os.path.join(out_dir, 'powercut.so')
    import subprocess
    subprocess.check_call([os.environ.get('CC', 'cc'), '-O2', '-shared', '-fPIC', '-o', lib, src, '-ldl', '-lpthread'])
    return lib


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('state')
    ap.add_argument('--mode', choices=('drop', 'keep'), default='drop')
    ap.add_argument('--seed', type=int, default=0)
    a = ap.parse_args()
    try:
        print(json.dumps(cut(a.state, a.mode, a.seed)))
    except ToolError as e:
        print(f'rebuild: {e}', file=sys.stderr)
        sys.exit(2)


if __name__ == '__main__':
    main()
