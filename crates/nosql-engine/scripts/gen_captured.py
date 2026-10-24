import json, re, sys
cap = sys.argv[1]; out = sys.argv[2]
src = open(cap + '/errors.py').read()
kw = {}
for m in re.finditer(r"case\('([^']+)',\s*\w+,\s*(\"(?:[^\"\\]|\\.)*\"|'[^']*'),?\s*(.*?)\)\s*$", src, re.M):
    cid, rest = m.group(1), m.group(3)
    d = {}
    for k, v in re.findall(r"(\w+)=('?[^,')]*'?)", rest):
        d[k] = v.strip("'")
    kw[cid] = d
s = json.load(open(cap + '/out/scylla-errors.json'))
c = json.load(open(cap + '/out/cassandra-errors.json'))
cm = {(x[0], x[2]): x[3] for x in c}
SKIP_CAT = {'setup', 'prepared', 'auth-off', 'consistency', 'rate-limit-timeout', 'lwt'}
setup = [q for q, r in s[0][3]]
def exp(r):
    if r is None: return None
    if r.get('ok'):
        return 'E::Ok(%s, &[%s])' % (rs(r.get('result_kind', 'Rows')), ', '.join(rs(w) for w in r.get('warnings', [])))
    e = r['error']
    code = e.get('code')
    if code is None: return None
    return 'E::Err(%s, %s)' % (code, rs(e['message']))
def rs(t):
    return 'r###"' + t + '"###'
lines = []
for cid, cat, q, r in s:
    if cat in SKIP_CAT or not q or ' with [' in q: continue
    if not re.match(r'^\s*([A-Z]|select|;|--|\s*$)', q): continue
    if q[:1].islower() and not q.startswith('select'): continue
    sr = exp(r); cr = exp(cm.get((cid, q)))
    if sr is None or cr is None: continue
    k = kw.get(cid, {})
    page = k.get('page_size', '5000')
    page = 'None' if page == 'None' else 'Some(%s)' % page
    cl = k.get('consistency', 'ONE')
    lines.append('    C { id: %s, cat: %s, cql: %s, page: %s, cl: %s, scylla: %s, cassandra: %s },' % (rs(cid), rs(cat), rs(q), page, rs(cl), sr, cr))
with open(out, 'w') as f:
    f.write('//! Cases generated from the reference error captures (`scylladb/scylla:2026.3.3`,\n//! `cassandra:5.0.9`): statement, then the code and message (or result kind and warnings) each\n//! reference answered. Run in order against the setup schema.\n\n')
    f.write('#![allow(clippy::all)]\n\nuse super::captured_harness::{C, E};\n\n')
    f.write('pub const SETUP: &[&str] = &[\n' + ''.join('    %s,\n' % rs(q) for q in setup) + '];\n\n')
    f.write('pub const CASES: &[C] = &[\n' + '\n'.join(lines) + '\n];\n')
print(len(lines))
