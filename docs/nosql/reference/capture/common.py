import sys, json, time, re
from cassandra.cluster import Cluster, ExecutionProfile, EXEC_PROFILE_DEFAULT, NoHostAvailable
from cassandra.auth import PlainTextAuthProvider
from cassandra.policies import WhiteListRoundRobinPolicy
from cassandra.query import SimpleStatement, tuple_factory

def connect(host='127.0.0.1', user=None, pw=None, proto=None, port=9042):
    kw = {}
    if user is not None: kw['auth_provider'] = PlainTextAuthProvider(user, pw)
    if proto: kw['protocol_version'] = proto
    prof = ExecutionProfile(request_timeout=120, row_factory=tuple_factory,
                            load_balancing_policy=WhiteListRoundRobinPolicy([host]))
    c = Cluster([host], port=port, execution_profiles={EXEC_PROFILE_DEFAULT: prof}, connect_timeout=30, **kw)
    return c, c.connect()

def err(e):
    if isinstance(e, NoHostAvailable) and e.errors:
        e = list(e.errors.values())[0]
    msg = getattr(e, 'message', None)
    if not isinstance(msg, str): msg = str(e)
    m = re.search(r'Error from server: code=([0-9a-fA-F]+) \[(.*?)\] message="(.*)"', msg, re.S)
    if m:
        return {'class': type(e).__name__, 'code': '0x' + m.group(1).upper().rjust(4, '0'), 'message': m.group(3)}
    code = getattr(e, 'code', None)
    return {'class': type(e).__name__, 'code': ('0x%04X' % code) if isinstance(code, int) else None, 'message': msg}

def typename(t):
    try: return t.cql_parameterized_type()
    except Exception: return str(t)

def run(s, q, params=None, keep_rows=True):
    try:
        st = SimpleStatement(q) if isinstance(q, str) else q
        rs = s.execute(st, params)
        out = {'ok': True, 'columns': list(rs.column_names or []),
               'types': [typename(t) for t in (rs.column_types or [])]}
        if keep_rows: out['rows'] = [list(r) for r in rs.current_rows]
        w = rs.response_future.warnings
        if w: out['warnings'] = list(w)
        return out
    except Exception as e:
        return {'ok': False, 'error': err(e)}
