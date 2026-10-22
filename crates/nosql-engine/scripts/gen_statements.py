# Generates crates/nosql-engine/src/tests/captured_statements.rs from the statements capture
# script (for the statement order) and its outputs (for the expected results).
import ast, json, sys
script, outdir, dest = sys.argv[1], sys.argv[2], sys.argv[3]
res = {}
for d in ('scylla', 'cassandra'):
    res[d] = {x[0]: x for x in json.load(open(f'{outdir}/{d}-statements.json'))}

tree = ast.parse(open(script).read())
steps = []  # ('setup', cql) | ('case', id, kind, cql, page)
ddl = []

def lit(n):
    return ast.literal_eval(n)

def call_name(n):
    return n.func.id if isinstance(n, ast.Call) and isinstance(n.func, ast.Name) else None

for node in tree.body:
    if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name) and node.targets[0].id == 'DDL':
        ddl = lit(node.value)
    elif isinstance(node, ast.For):
        body = node.body[0]
        if isinstance(node.iter, ast.Name) and node.iter.id == 'DDL':
            for cid, x in ddl:
                steps.append(('case', cid, 'raw', x, None))
        elif isinstance(node.iter, ast.List) and isinstance(body, ast.Expr):
            fn = call_name(body.value)
            kind = {'q': 'lwt', 'prep': 'prep'}[fn]
            for cid, x in lit(node.iter):
                steps.append(('case', cid, kind, x, None))
    elif isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
        fn = call_name(node.value)
        args = node.value.args
        if fn == 'setup':
            stmts = []
            for a in args:
                if isinstance(a, ast.Starred):
                    v = a.value
                    # [x for x in [...]]
                    if isinstance(v, ast.ListComp):
                        stmts += lit(v.generators[0].iter)
                    else:
                        stmts += lit(v)
                else:
                    stmts.append(lit(a))
            for x in stmts:
                steps.append(('setup', x))
        elif fn == 'q':
            page = None
            for k in node.value.keywords:
                if k.arg == 'fetch_size':
                    page = lit(k.value)
            steps.append(('case', lit(args[0]), 'query', lit(args[2]), page))
        elif fn == 'wr':
            stmts = [lit(a) for a in args[1:]]
            for x in stmts[:-1]:
                steps.append(('setup', x))
            steps.append(('case', lit(args[0]), 'query', stmts[-1], None))

def canon(kind, r, page):
    if not r['ok']:
        e = r['error']
        if e['class'] == 'DriverException' and 'raw' in r:
            raw = r['raw']
            return f"RAW {raw['result_kind']} n={raw.get('column_count')}"
        return f"ERR {e['code']} {e['message']}"
    w = r.get('warnings') or []
    if kind == 'prep':
        return f"PREP bind={r['bind']!r} result={r['result']!r} rk={r['routing_key_indexes']!r} lwt={r['is_lwt']!r}"
    if 'result_kind' in r and 'columns' not in r:
        rk = r['result_kind']
        if rk == 'SchemaChange':
            s = 'OK SchemaChange ' + ' '.join(r['schema_change'])
        elif rk == 'SetKeyspace':
            s = 'OK SetKeyspace ' + r['keyspace']
        else:
            s = 'OK ' + rk
        return s + (f" WARN {w!r}" if w else '')
    s = f"ROWS cols={r['columns']!r} types={r['types']!r} rows={r['rows']!r}"
    if w:
        s += f" WARN {w!r}"
    if page is not None:
        s += f" more={r.get('has_more_pages')!r}"
    return s

def rs(s):
    return 'r###"' + s + '"###'

out = ['//! Generated from the reference statement captures (`docs/nosql/reference/capture/statements.py`',
       '//! run on both images) by `gen_statements.py`; do not edit by hand. Expected results are in the',
       '//! canonical text form of `super::captured_statements_harness::canon`.', '',
       'use super::captured_statements_harness::S;', '',
       'pub const STEPS: &[S] = &[']
for st in steps:
    if st[0] == 'setup':
        out.append(f'    S::Setup({rs(st[1])}),')
        continue
    _, cid, kind, cql, page = st
    if cid not in res['scylla'] or cid not in res['cassandra']:
        continue
    sc = canon(kind, res['scylla'][cid][3], page)
    cc = canon(kind, res['cassandra'][cid][3], page)
    pg = f'Some({page})' if page else 'None'
    out.append(f'    S::Case {{ id: {rs(cid)}, kind: {rs(kind)}, cql: {rs(cql)}, page: {pg}, scylla: {rs(sc)}, cassandra: {rs(cc)} }},')
out.append('];')
open(dest, 'w').write('\n'.join(out) + '\n')
print(len(steps))
