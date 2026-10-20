#!/usr/bin/env python3
"""Records friso's traditional -> simplified table (`Cn_T2S`) from a reference server's
observable behaviour and writes `t2s.tsv`.

The table is derived black-box, like the rest of roll: the reference (`redis:8-alpine` with its
bundled RediSearch, `--enable-debug-command yes`) and a *base* roll server whose table is empty
index the same LANGUAGE chinese documents, and every difference in the terms they produce is a
conversion. Nothing of the module's own data is used.

1. One document per BMP code point (surrogates excepted). A code point whose term on the
   reference is a different single character than on the base converts to that character.
2. The rest are probed in context: one document per code point holding dictionary words with a
   character that is not a word of its own (only reachable inside words) replaced by the code
   point, plus name, number, Latin, escape and punctuation contexts. A code point whose terms
   differ converts either to a probe word's character (the reference forms that word), or to a
   character no context can tell apart from a stand-in: U+00B7 for a 2-byte target, U+3400 (no
   CJK character to the segmenter) or U+4E02 (a CJK character in no lexicon) for a 3-byte one;
   the stand-in is recorded, checked on the reference context by context (a mixed word such as
   "c#" keeps before a non-CJK character, not before a CJK one).

`--verify` (the base built with the table) expects every code point alone and in context to
index the same terms on both servers.

Regenerate:
    1. replace t2s.tsv by its header lines (an empty table) and start roll alone:
       ROLL_COMPAT_SERVE=127.0.0.1:55791 cargo test -p redis-server --test compat serve -- --ignored
    2. docker run -d --name t2s-ref -p 127.0.0.1:55790:6379 redis:8-alpine \
           redis-server --enable-debug-command yes
    3. python3 gen_t2s.py --ref 127.0.0.1:55790 --base 127.0.0.1:55791
Both servers are flushed.
"""
import argparse
import glob
import os
import socket
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DICT = os.path.join(HERE, 'dict')
STANDIN2 = 0x00B7
STANDINS3 = (0x3400, 0x4E02)
CONTEXTS = ['王%s', '欧阳%s', '王%s明', '王明%s', '老%s', '%s王', '小王%s', '3%s', '%s3', '3.5%s', 'b%s', '%sb', 'ok%s',
            '中%s', '%s中', '%s中国', '中国%s', 'a%sb', '%s%s', '%s人民', '人%s民', '%s的', '的%s', 'x%s超', '卡拉%s',
            '%s\\,a', 'a\\,%s', '%s@', '@%s', '#%s#', '%s.%s', '1%s2', '张%s%s', '王%s%s明', 'c#%s', 'c++%s',
            '%sc#', 'b超%s', '卡拉ok%s']


class Conn:
    def __init__(self, addr):
        host, port = addr.rsplit(':', 1)
        self.s = socket.create_connection((host, int(port)))
        self.f = self.s.makefile('rb', buffering=1 << 20)

    @staticmethod
    def enc(args):
        out = [b'*%d\r\n' % len(args)]
        for a in args:
            a = a.encode('utf-8') if isinstance(a, str) else a if isinstance(a, bytes) else str(a).encode()
            out.append(b'$%d\r\n%s\r\n' % (len(a), a))
        return b''.join(out)

    def read(self):
        line = self.f.readline()
        t, rest = line[:1], line[1:-2]
        if t == b'+':
            return rest.decode()
        if t == b'-':
            return RuntimeError(rest.decode())
        if t == b':':
            return int(rest)
        if t == b'$':
            n = int(rest)
            return None if n < 0 else self.f.read(n + 2)[:-2]
        if t == b'*':
            n = int(rest)
            return None if n < 0 else [self.read() for _ in range(n)]
        raise ValueError(line)

    def cmd(self, *args):
        self.s.sendall(self.enc(args))
        return self.read()

    def pipe(self, cmds, chunk=500):
        res = []
        for i in range(0, len(cmds), chunk):
            part = cmds[i:i + chunk]
            self.s.sendall(b''.join(self.enc(c) for c in part))
            res.extend(self.read() for _ in part)
        return res


def index_terms(conn, docs):
    """Index `docs` (LANGUAGE chinese, NOSTEM) on an empty keyspace; document n -> its terms."""
    # A leading document resets the segmenter state the previous text leaves behind.
    docs = ['中 a'] + list(docs)
    conn.cmd('FLUSHALL')
    assert conn.cmd('FT.CREATE', 'z', 'ON', 'HASH', 'PREFIX', '1', 'z:', 'LANGUAGE', 'chinese',
                    'SCHEMA', 't', 'TEXT', 'NOSTEM') == 'OK'
    assert all(x == 1 for x in conn.pipe([('HSET', 'z:%d' % n, 't', d) for n, d in enumerate(docs)], 200))
    terms = conn.cmd('_FT.DEBUG', 'DUMP_TERMS', 'z')
    out = {}
    for t, ids in zip(terms, conn.pipe([('_FT.DEBUG', 'DUMP_INVIDX', 'z', t) for t in terms])):
        if isinstance(ids, list):  # terms that aren't valid UTF-8 can't be looked up
            for d in ids:
                if d > 1:
                    out.setdefault(d - 2, set()).add(t)
    conn.cmd('FT.DROPINDEX', 'z')
    conn.cmd('FLUSHALL')
    return out


def both(ref, base, docs, chunk=1000):
    r, b = {}, {}
    for s in range(0, len(docs), chunk):
        for conn, acc in ((ref, r), (base, b)):
            for n, ts in index_terms(conn, docs[s:s + chunk]).items():
                acc[s + n] = ts
    return r, b


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--ref', required=True)
    ap.add_argument('--base', required=True)
    ap.add_argument('--out', default=os.path.join(HERE, 't2s.tsv'))
    ap.add_argument('--verify', action='store_true', help='the base has the table: expect no differences')
    a = ap.parse_args()
    ref, base = Conn(a.ref), Conn(a.base)
    if a.verify:
        return verify(ref, base)

    cps = [c for c in range(0x10000) if not 0xD800 <= c <= 0xDFFF]
    r, b = both(ref, base, [chr(c) for c in cps], chunk=len(cps))
    table, how = {}, {}
    rest = []
    for n, c in enumerate(cps):
        x, y = r.get(n, set()), b.get(n, set())
        if x == y:
            if not x:
                rest.append(c)
            continue
        if len(x) == 1 and len(next(iter(x)).decode()) == 1 and y in (set(), {chr(c).encode()}):
            table[c] = ord(next(iter(x)).decode())
            how[c] = 'single'
        elif not x:
            rest.append(c)  # a word on the base, none on the reference: the target isn't a word
        else:
            sys.exit('unexpected: U+%04X ref %r base %r' % (c, x, y))
    print('single-character conversions:', len(table), file=sys.stderr)

    # Probe words: dictionary words with one character that is no word of its own on the base,
    # the others neither converted nor such characters; kept when the base forms the word.
    single = {chr(cps[n]) for n, ts in b.items() if ts}
    words = []
    for f in sorted(glob.glob(os.path.join(DICT, '*.lex'))):
        for line in open(f, encoding='utf-8', errors='replace'):
            w = line.strip().split('/')[0]
            if 2 <= len(w) <= 6 and all(len(ch.encode()) == 3 for ch in w):
                words.append(w)
    at = {c: n for n, c in enumerate(cps)}
    keys = {chr(c) for c in table} | {chr(c) for c in rest if b.get(at[c])}
    cands = {}
    for w in sorted(set(words), key=lambda w: (len(w), w)):
        for i, ch in enumerate(w):
            if ch not in single and all(o not in keys for o in w[:i] + w[i + 1:]):
                if len(cands.setdefault(ch, [])) < 20:
                    cands[ch].append((w, i))
    flat = [(t, w, i) for t, ws in sorted(cands.items()) for (w, i) in ws]
    got = index_terms(base, [w for (_, w, _) in flat])
    probe = {}
    for n, (t, w, i) in enumerate(flat):
        if t not in probe and w.encode() in got.get(n, ()):
            probe[t] = (w, i)
    print('probe words:', len(probe), file=sys.stderr)

    def doc(ch):
        ws = [w[:i] + ch + w[i + 1:] for t, (w, i) in sorted(probe.items()) if t != ch]
        return ' '.join(ws + [x.replace('%s', ch) for x in CONTEXTS])

    r, b = both(ref, base, [doc(chr(c)) for c in rest])
    diff = [c for n, c in enumerate(rest) if r.get(n, set()) != b.get(n, set())]
    print('context differences:', len(diff), file=sys.stderr)
    standin = {}
    at = {c: n for n, c in enumerate(rest)}
    for c in diff:
        n = at[c]
        found = {t for t, (w, i) in probe.items() if w.encode() in r[n] - b.get(n, set()) and chr(c) not in w}
        # The probe word must form from c alone.
        found = {t for t in found if (lambda w, i: w.encode() in index_terms(ref, [w[:i] + chr(c) + w[i + 1:]]).get(0, ()))(*probe[t])}
        if len(found) == 1:
            table[c] = ord(found.pop())
            how[c] = 'context'
            continue
        if found:
            sys.exit('ambiguous: U+%04X -> %s' % (c, found))
        standin[c] = None
    # Unidentifiable targets. The target's UTF-8 length shows: the segmenter advances by the
    # converted character's length, so a shorter target leaves it inside the code point's bytes
    # and the next character is lost ("c中国" indexes 国 alone). A 3-byte target is checked
    # against the stand-in on the reference context by context; for a 2-byte one only the length
    # is observable (every 2-byte character is of the same class), `--verify` checks the result.
    k = len(standin)
    docs = [doc(chr(c)) for c in standin] + [doc(chr(x)) for x in STANDINS3]
    got = index_terms(ref, docs)
    own = index_terms(base, docs[k:])
    assert all(got.get(k + m) == own.get(m) for m in range(len(STANDINS3))), 'a stand-in converts'
    blank = lambda ts, ch: {t.replace(chr(ch).encode(), b'\0') for t in ts}
    for j, c in enumerate(standin):
        lost = index_terms(ref, [chr(c) + '中国']).get(0, set())
        if lost == {'国'.encode()}:
            table[c] = STANDIN2
        elif lost == {'中国'.encode()}:
            mine = blank(got.get(j, set()), c)
            same = [x for m, x in enumerate(STANDINS3) if blank(got.get(k + m, set()), x) == mine]
            if not same:
                sys.exit('U+%04X converts to something no stand-in reproduces' % c)
            table[c] = same[0]
        else:
            sys.exit('U+%04X converts to something no stand-in reproduces' % c)
        how[c] = 'stand-in'
    counts = {h: sum(1 for v in how.values() if v == h) for h in ('single', 'context', 'stand-in')}
    print(counts, file=sys.stderr)

    with open(a.out, 'w', encoding='utf-8') as f:
        f.write('# friso\'s traditional -> simplified conversions (`Cn_T2S`) as Redis 8.10.2 (redis:8-alpine,\n'
                '# bundled RediSearch 8.10.1) applies them to LANGUAGE chinese text, recorded from its\n'
                '# observable behaviour by gen_t2s.py (see there to regenerate); no module data was used.\n'
                '# TRAD<TAB>SIMP (hexadecimal code points), then how the entry was observed:\n'
                '#   single    a one-character document indexes the simplified character\n'
                '#   context   only inside a dictionary word (the simplified character is no word alone)\n'
                '#   stand-in  the target is unobservable (no word, no context tells it apart): a\n'
                '#             character of the same UTF-8 length that behaves identically\n'
                '# %d entries: %d single, %d context, %d stand-in.\n' % (len(table), counts['single'], counts['context'], counts['stand-in']))
        for c in sorted(table):
            f.write('%04X\t%04X\t%s\t%s\t%s\n' % (c, table[c], chr(c), chr(table[c]), how[c]))


def verify(ref, base):
    """Every code point alone and in contexts indexes the same terms on both servers."""
    cps = [c for c in range(0x10000) if not 0xD800 <= c <= 0xDFFF]
    words = set()
    for f in sorted(glob.glob(os.path.join(DICT, '*.lex'))):
        for line in open(f, encoding='utf-8', errors='replace'):
            w = line.strip().split('/')[0]
            if 2 <= len(w) <= 6 and all(len(ch.encode()) == 3 for ch in w):
                words.add(w)
    sample = sorted(words)[::1700]
    bad = 0
    for docs in ([chr(c) for c in cps],
                 [' '.join([w[:i] + chr(c) + w[i + 1:] for w in sample for i in range(1, len(w))]
                           + [x.replace('%s', chr(c)) for x in CONTEXTS]) for c in cps]):
        r, b = both(ref, base, docs, chunk=2000)
        for n, c in enumerate(cps):
            if r.get(n, set()) != b.get(n, set()):
                bad += 1
                if bad <= 20:
                    print('U+%04X' % c, sorted(r.get(n, set()) - b.get(n, set())), sorted(b.get(n, set()) - r.get(n, set())))
    print('differences:', bad)
    sys.exit(1 if bad else 0)


if __name__ == '__main__':
    main()
