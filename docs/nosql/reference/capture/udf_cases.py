# UDF / UDA cases for udf_live.py: BATTERIES = [(name, dialects, setup, [(id, cql)])]. Setup
# statements run first (their answers aren't kept); a battery's CREATE KEYSPACE is dropped at the
# end. Keyspaces use the udfv_ prefix.
#
# Never add cases that crash a reference. Known crashes: on ScyllaDB 2026.3.3 a Lua error object
# that isn't a string or number (`error({})`, probably `error()`/`error(nil)`), and returning a
# non-table where a set or a UDT (probably also a map) is expected, segfault the node. On
# Cassandra 5.0.9 a Java UDF running past user_defined_functions_fail_timeout (1500 ms) stops the
# VM (`user_function_timeout_policy: die`); keep Java bodies short and allocation-free. Untested
# for the same reason: Lua functions, threads or decimals converted to inet/uuid/text-like
# results, and timeouts inside non-yieldable C calls (table.sort / string.gsub callbacks).

NTS = "{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}"


def ks(name):
    return "CREATE KEYSPACE %s WITH replication = %s" % (name, NTS)


# A Lua body describing argument x (tables with sorted keys, decimals by their type name).
DESC = ('local function d(v) if type(v) == "userdata" then return getmetatable(v).__name end '
        'if type(v) ~= "table" then return tostring(v) end local t = {} '
        'for k, e in pairs(v) do t[#t+1] = tostring(k) .. "=" .. d(e) end table.sort(t) '
        'return "{" .. table.concat(t, ",") .. "}" end return type(x) .. ":" .. d(x)')

TYPES = [('ti', 'tinyint'), ('si', 'smallint'), ('i', 'int'), ('bi', 'bigint'), ('f', 'float'), ('d', 'double'),
         ('vi', 'varint'), ('de', 'decimal'), ('dt', 'date'), ('tm', 'time'), ('ts', 'timestamp'),
         ('du', 'duration'), ('ip', 'inet'), ('u', 'uuid'), ('tu', 'timeuuid'), ('b', 'blob'), ('bo', 'boolean'),
         ('a', 'ascii'), ('tx', 'text'), ('l', 'list<int>'), ('s', 'set<int>'), ('m', 'map<text, int>'),
         ('tp', 'tuple<int, text>'), ('ud', 'u'), ('ve', 'vector<float, 2>'), ('ll', 'list<frozen<list<int>>>'),
         ('ms', 'map<int, frozen<set<text>>>'), ('fl', 'frozen<list<text>>')]


def lua_args():
    k = 'udfv_args'
    setup = [ks(k), 'CREATE TYPE %s.u (a int, b text)' % k,
             'CREATE TABLE %s.all (pk int PRIMARY KEY, %s)' % (k, ', '.join('%s %s' % (n, t if t != 'u' else 'frozen<u>') for n, t in TYPES)),
             "INSERT INTO %s.all (pk, ti, si, i, bi, f, d, vi, de, dt, tm, ts, du, ip, u, tu, b, bo, a, tx, l, s, m, tp, ud, ve, ll, ms, fl) VALUES "
             "(1, 1, 2, 3, 9007199254740993, 1.5, 2.25, 123456789012345678901234567890, 12345678901234567890.123456789, '2020-01-02', "
             "'01:02:03.123456789', '2020-01-02 03:04:05.678+0000', 1mo2d3ns, '127.0.0.1', 11111111-2222-3333-4444-555555555555, "
             "8e14e760-7fa8-11eb-bc66-000000000001, 0xcafe, true, 'asc', 'txt', [3, 1, 2], {5, 4}, {'a': 1, 'b': 2}, (7, 'x'), "
             "{a: 1, b: 'q'}, [1.5, 2.5], [[1], [2, 3]], {1: {'x'}}, ['p', 'q'])" % k,
             "INSERT INTO %s.all (pk, ti, si, i, bi, f, d, vi, de, dt, tm, ts, du, ip, u, tu, b, bo, a, tx, tp, ud, ve) VALUES "
             "(2, -128, -32768, -2147483648, -9223372036854775808, -0.0, 2.0, -1, 1.5E+3, '-5877641-06-23', "
             "'23:59:59.999999999', '1900-01-01 00:00:00+0000', -1mo2d3ns, '::ffff:1.2.3.4', 00000000-0000-0000-0000-000000000000, "
             "00000000-0000-1000-8000-000000000000, 0x, false, '', 'é😀', (null, null), {a: null, b: 'z'}, [-0.0, 3.0])" % k,
             "INSERT INTO %s.all (pk, d, ip, de, vi) VALUES (3, 1e308, '2001:db8::1', -0.000, 0)" % k,
             "INSERT INTO %s.all (pk, d) VALUES (4, NaN)" % k,
             "INSERT INTO %s.all (pk, d) VALUES (5, -Infinity)" % k,
             'INSERT INTO %s.all (pk) VALUES (6)' % k]
    cases = []
    for n, t in TYPES:
        cases.append(('desc_%s_fn' % n, "CREATE FUNCTION %s.desc_%s(x %s) CALLED ON NULL INPUT RETURNS text LANGUAGE lua AS '%s'" % (k, n, t, DESC)))
        cases.append(('desc_%s' % n, 'SELECT pk, %s.desc_%s(%s) FROM %s.all' % (k, n, n, k)))
        cases.append(('same_%s_fn' % n, "CREATE FUNCTION %s.same_%s(x %s) CALLED ON NULL INPUT RETURNS %s LANGUAGE lua AS 'return x'" % (k, n, t, t)))
        cases.append(('same_%s' % n, 'SELECT pk, %s.same_%s(%s) FROM %s.all' % (k, n, n, k)))
    for n in ('vi', 'de'):
        t = dict(TYPES)[n]
        cases.append(('text_%s_fn' % n, "CREATE FUNCTION %s.text_%s(x %s) CALLED ON NULL INPUT RETURNS text LANGUAGE lua AS 'return x'" % (k, n, t)))
        cases.append(('text_%s' % n, 'SELECT pk, %s.text_%s(%s) FROM %s.all' % (k, n, n, k)))
    for lit in ('1.50', '1.5E+3', '-0.000', '0.00012', '1E-10', '-7', '123456789012345678901234567890.5'):
        cases.append(('text_de_lit %s' % lit, 'SELECT %s.text_de(%s) FROM %s.all WHERE pk = 1' % (k, lit, k)))
    cases.append(('null_rnoni_fn', "CREATE FUNCTION %s.rnoni(x int) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE lua AS 'return tostring(x)'" % k))
    cases.append(('null_rnoni', 'SELECT pk, %s.rnoni(i) FROM %s.all' % (k, k)))
    cases.append(('varargs_fn', "CREATE FUNCTION %s.va(x int, y text) CALLED ON NULL INPUT RETURNS text LANGUAGE lua AS 'return select(\"#\", ...) .. \":\" .. tostring(x) .. \":\" .. tostring(y) .. \":\" .. tostring((...))'" % k))
    cases.append(('varargs', 'SELECT pk, %s.va(i, tx) FROM %s.all' % (k, k)))
    cases.append(('empty_list_arg', 'SELECT %s.desc_l([]) FROM %s.all WHERE pk = 1' % (k, k)))
    cases.append(('empty_set_arg', 'SELECT %s.desc_s({}) FROM %s.all WHERE pk = 1' % (k, k)))
    cases.append(('empty_frozen_arg', 'SELECT %s.desc_fl([]) FROM %s.all WHERE pk = 1' % (k, k)))
    cases.append(('tuple_literal_udt', "SELECT %s.desc_ud((5, 'w')) FROM %s.all WHERE pk = 1" % (k, k)))
    cases.append(('null_udt_field', 'SELECT %s.desc_ud({a: 1}) FROM %s.all WHERE pk = 1' % (k, k)))
    cases.append(('literal_args', "SELECT %s.desc_i(5), %s.desc_tx('lit'), %s.desc_d(1e30), %s.desc_bi(-1) FROM %s.all WHERE pk = 1" % (k, k, k, k, k)))
    return ('lua_args', {'scylla'}, setup, cases)


# (id, return type, Lua body) for the result conversions.
RETURNS = [
    # integers
    ('int_int', 'int', 'return 7'), ('int_wrap', 'int', 'return 2147483648'), ('int_float_integral', 'int', 'return 3.0'),
    ('int_float_frac', 'int', 'return 1.5'), ('int_2p31', 'int', 'return 2^31'), ('int_2p63', 'int', 'return 2^63'),
    ('int_neg_huge', 'int', 'return -1e30'), ('int_nan', 'int', 'return 0/0'), ('int_inf', 'int', 'return 1/0'),
    ('int_str', 'int', 'return "42"'), ('int_str_space', 'int', 'return " 12 "'), ('int_str_hex', 'int', 'return "0x10"'),
    ('int_str_float', 'int', 'return "1.0"'), ('int_str_frac', 'int', 'return "1.5"'), ('int_str_big', 'int', 'return "2147483648"'),
    ('int_str_bad', 'int', 'return "abc"'), ('int_str_empty', 'int', 'return ""'), ('int_table', 'int', 'return {}'),
    ('int_bool', 'int', 'return true'), ('int_nil', 'int', 'return nil'), ('int_none', 'int', 'local y = 1'),
    ('int_two', 'int', 'return 1, 2'), ('tinyint_wrap', 'tinyint', 'return 300'), ('smallint_wrap', 'smallint', 'return -40000'),
    ('bigint_big', 'bigint', 'return 9007199254740993'), ('bigint_2p63', 'bigint', 'return 2^63'),
    ('bigint_str_2p63', 'bigint', 'return "9223372036854775808"'), ('bigint_str_huge', 'bigint', 'return "99999999999999999999"'),
    ('bigint_overflow', 'bigint', 'return 9223372036854775807 + 1'), ('bigint_neg_huge', 'bigint', 'return -1e30'),
    ('counter_ret', 'counter', 'return 5'),
    # floats
    ('float_int', 'float', 'return 3'), ('float_huge', 'float', 'return 1e300'), ('float_str', 'float', 'return "1.5"'),
    ('double_big_int', 'double', 'return 9007199254740993'), ('double_str_bad', 'double', 'return "x"'),
    ('double_table', 'double', 'return {}'), ('double_bool', 'double', 'return false'), ('double_str_exp', 'double', 'return "1e300"'),
    # varint / decimal
    ('varint_int', 'varint', 'return -7'), ('varint_float_integral', 'varint', 'return 3.0'), ('varint_float_frac', 'varint', 'return 1.5'),
    ('varint_2p70', 'varint', 'return 2^70'), ('varint_2p63', 'varint', 'return 2^63'), ('varint_str_big', 'varint', 'return "123456789012345678901234567890123"'),
    ('varint_str_exp', 'varint', 'return "1e3"'), ('varint_str_float', 'varint', 'return "1.0"'), ('varint_str_space', 'varint', 'return " 5"'),
    ('varint_str_huge', 'varint', 'return "99999999999999999999"'), ('varint_str_bad', 'varint', 'return "abc"'),
    ('varint_table', 'varint', 'return {}'), ('varint_bool', 'varint', 'return true'),
    ('decimal_int', 'decimal', 'return 7'), ('decimal_float_integral', 'decimal', 'return 7.0'), ('decimal_float_frac', 'decimal', 'return 1.5'),
    ('decimal_point_one', 'decimal', 'return 0.1'), ('decimal_2p63', 'decimal', 'return 2^63'), ('decimal_str_exp', 'decimal', 'return "1.25e3"'),
    ('decimal_str_negexp', 'decimal', 'return "1.5E-3"'), ('decimal_str_hex', 'decimal', 'return "0x10"'), ('decimal_str_space', 'decimal', 'return " 1.5 "'),
    ('decimal_str_scale', 'decimal', 'return "-1.50"'), ('decimal_str_100', 'decimal', 'return "100.0"'), ('decimal_str_huge', 'decimal', 'return "1e400"'),
    ('decimal_str_int', 'decimal', 'return "5"'), ('decimal_str_negzero', 'decimal', 'return "-0.0"'), ('decimal_str_dot', 'decimal', 'return ".5"'),
    ('decimal_str_plus', 'decimal', 'return "+7.25"'), ('decimal_str_frac', 'decimal', 'return "1.5"'), ('decimal_str_bad', 'decimal', 'return "abc"'),
    ('decimal_str_empty', 'decimal', 'return ""'), ('decimal_table', 'decimal', 'return {}'), ('decimal_bool', 'decimal', 'return true'),
    # text-likes
    ('text_int', 'text', 'return -5'), ('text_float_integral', 'text', 'return 2.0'), ('text_float', 'text', 'return 1.5'),
    ('text_1e30', 'text', 'return 1e30'), ('text_nan', 'text', 'return 0/0'), ('text_pos_nan', 'text', 'return -(0/0)'),
    ('text_neg_inf', 'text', 'return -1/0'), ('text_neg_zero', 'text', 'return -0.0'), ('text_point_one', 'text', 'return 0.1'),
    ('text_sum', 'text', 'return 0.1 + 0.2'), ('text_third', 'text', 'return 1/3'), ('text_1e15', 'text', 'return 1e15'),
    ('text_1e16', 'text', 'return 1e16'), ('text_1e19', 'text', 'return 1e19'), ('text_2p63', 'text', 'return 2^63'),
    ('text_small', 'text', 'return 1e-5'), ('text_1e_4', 'text', 'return 1e-4'), ('text_1_2345e20', 'text', 'return 1.2345e20'),
    ('text_denormal', 'text', 'return 5e-324'), ('text_neg_small', 'text', 'return -2.5e-7'), ('text_max', 'text', 'return 1.7976931348623157e308'),
    ('text_frac_big', 'text', 'return 1234567890123456.5'), ('text_bool', 'text', 'return true'), ('text_table', 'text', 'return {}'),
    ('text_bad_utf8', 'text', 'return "a\\255"'), ('text_utf8', 'text', 'return "é"'), ('ascii_bad', 'ascii', 'return "é"'),
    ('ascii_float', 'ascii', 'return 1.5'), ('blob_int', 'blob', 'return 12'), ('blob_float', 'blob', 'return 1.5'),
    ('blob_float_integral', 'blob', 'return 2.0'), ('blob_table', 'blob', 'return {}'), ('blob_bool', 'blob', 'return false'),
    ('blob_bytes', 'blob', 'return "\\0\\1\\255"'),
    # booleans
    ('bool_true', 'boolean', 'return true'), ('bool_false', 'boolean', 'return false'), ('bool_one', 'boolean', 'return 1'),
    ('bool_zero', 'boolean', 'return 0'), ('bool_str', 'boolean', 'return "false"'), ('bool_table', 'boolean', 'return {}'),
    # temporal
    ('ts_str', 'timestamp', 'return "2020-01-02 03:04:05+0000"'), ('ts_float_frac', 'timestamp', 'return 1.5'),
    ('ts_float_integral', 'timestamp', 'return 3.0'), ('ts_str_num', 'timestamp', 'return "123"'), ('ts_str_space', 'timestamp', 'return " 12"'),
    ('ts_str_frac', 'timestamp', 'return "1.5"'), ('ts_str_bad', 'timestamp', 'return "garbage"'), ('ts_empty_table', 'timestamp', 'return {}'),
    ('ts_bool', 'timestamp', 'return true'), ('ts_2p63', 'timestamp', 'return 2^63'), ('ts_str_huge', 'timestamp', 'return "99999999999999999999"'),
    ('ts_neg', 'timestamp', 'return -5'),
    ('ts_table', 'timestamp', 'return {year = 2020, month = 1, day = 2, hour = 3, min = 4, sec = 5}'),
    ('ts_table_minute', 'timestamp', 'return {year = 2020, month = 1, day = 2, hour = 3, minute = 4}'),
    ('ts_table_isdst', 'timestamp', 'return {year = 2020, month = 1, day = 2, isdst = false}'),
    ('ts_table_wday', 'timestamp', 'return {year = 2020, month = 1, day = 2, wday = 3}'),
    ('ts_table_sec70', 'timestamp', 'return {year = 2020, month = 1, day = 2, sec = 70}'),
    ('ts_table_date', 'timestamp', 'return {year = 2020, month = 1, day = 2}'),
    ('date_str', 'date', 'return "2020-01-02"'), ('date_table', 'date', 'return {year = 2020, month = 1, day = 2}'),
    ('date_month13', 'date', 'return {year = 2020, month = 13, day = 2}'), ('date_feb30', 'date', 'return {year = 2020, month = 2, day = 30}'),
    ('date_no_day', 'date', 'return {year = 2020, month = 2}'), ('date_hour', 'date', 'return {year = 2020, month = 2, day = 3, hour = 4}'),
    ('date_str_fields', 'date', 'return {year = "2020", month = 2.0, day = 3}'), ('date_frac_year', 'date', 'return {year = 2020.5, month = 2, day = 3}'),
    ('date_epoch', 'date', 'return 2147483648'), ('date_neg', 'date', 'return -1'), ('date_2p32', 'date', 'return 4294967296'),
    ('date_str_bad', 'date', 'return "bad"'), ('date_float', 'date', 'return 1.5'), ('date_seq', 'date', 'return {1, 2}'),
    ('date_str_num', 'date', 'return "12"'), ('date_float_integral', 'date', 'return 3.0'), ('date_bool', 'date', 'return true'),
    ('date_isdst', 'date', 'return {year = 2020, month = 1, day = 2, isdst = true}'), ('date_far', 'date', 'return {year = -5000000, month = 1, day = 2}'),
    ('date_zero', 'date', 'return {year = 2020, month = 0, day = 0}'), ('date_day_table', 'date', 'return {year = 2020, month = 1, day = {}}'),
    ('date_day_str', 'date', 'return {year = 2020, month = 1, day = "x"}'), ('date_day_big', 'date', 'return {year = 2020, month = 1, day = 99999999999}'),
    ('date_year_small', 'date', 'return {year = -99999999999, month = 1, day = 1}'), ('date_month_big', 'date', 'return {year = 1, month = 3000000000, day = 1}'),
    ('time_str', 'time', 'return "01:02:03.5"'), ('time_int', 'time', 'return 5'), ('time_day', 'time', 'return 86400000000000'),
    ('time_neg', 'time', 'return -1'), ('time_str_bad_hour', 'time', 'return "25:00:00"'), ('time_table', 'time', 'return {}'),
    ('time_float', 'time', 'return 2.5'), ('time_str_num', 'time', 'return "12"'), ('time_float_integral', 'time', 'return 3.0'),
    ('time_bool', 'time', 'return true'), ('time_str_bad', 'time', 'return "bad"'),
    ('dur_table', 'duration', 'return {months = 1, days = 2, nanoseconds = 3}'), ('dur_months', 'duration', 'return {months = 1}'),
    ('dur_mixed', 'duration', 'return {months = 1, days = -2, nanoseconds = 3}'), ('dur_weeks', 'duration', 'return {months = 1, weeks = 2}'),
    ('dur_str', 'duration', 'return "1h30m"'), ('dur_str_bad', 'duration', 'return "bad"'), ('dur_int', 'duration', 'return 5'),
    ('dur_frac', 'duration', 'return {months = 1.5}'), ('dur_big', 'duration', 'return {months = 4294967297}'),
    ('dur_big_days', 'duration', 'return {days = -4294967297}'), ('dur_empty', 'duration', 'return {}'), ('dur_seq', 'duration', 'return {[1] = 2}'),
    ('dur_bool', 'duration', 'return true'), ('dur_neg', 'duration', 'return {months = -1, days = -2, nanoseconds = -3}'),
    ('dur_str_field', 'duration', 'return {months = "2"}'),
    # inet / uuids
    ('inet_v6', 'inet', 'return "::1"'), ('inet_v4', 'inet', 'return "10.0.0.1"'), ('inet_bad', 'inet', 'return "bad"'),
    ('inet_int', 'inet', 'return 5'), ('inet_table', 'inet', 'return {}'), ('inet_bool', 'inet', 'return true'),
    ('uuid_ok', 'uuid', 'return "11111111-2222-3333-4444-555555555555"'), ('uuid_upper', 'uuid', 'return "AAAAAAAA-2222-3333-4444-555555555555"'),
    ('uuid_bad', 'uuid', 'return "bad"'), ('uuid_int', 'uuid', 'return 5'), ('uuid_table', 'uuid', 'return {}'),
    ('timeuuid_v4', 'timeuuid', 'return "11111111-2222-3333-4444-555555555555"'), ('timeuuid_ok', 'timeuuid', 'return "8e14e760-7fa8-11eb-bc66-000000000001"'),
    # collections (tables only: non-tables for sets, maps and UDTs crash the reference)
    ('list_ok', 'list<int>', 'return {3, 1, 2}'), ('list_empty', 'list<int>', 'return {}'), ('list_hole', 'list<int>', 'return {1, nil, 3}'),
    ('list_named', 'list<int>', 'return {1, 2, x = 3}'), ('list_from2', 'list<int>', 'return {[2] = 1}'), ('list_gap', 'list<int>', 'return {[1] = 5, [3] = 6}'),
    ('list_float_key', 'list<int>', 'return {[1.0] = 5}'), ('list_str_key', 'list<int>', 'return {["1"] = 5}'), ('list_int', 'list<int>', 'return 5'),
    ('list_str', 'list<int>', 'return "ab"'), ('list_bad_elem', 'list<int>', 'return {"x"}'), ('list_zero_key', 'list<int>', 'return {[0] = 1, 2}'),
    ('list_frac_elem', 'list<int>', 'return {1, 2.5}'), ('list_text', 'list<text>', 'return {1, 2.5, "x"}'), ('list_bool', 'list<int>', 'return false'),
    ('list_nested', 'list<frozen<list<int>>>', 'return {{1}, {}}'), ('list_frac_key', 'list<int>', 'return {[1.5] = 1}'),
    ('set_ok', 'set<int>', 'return {[3] = true, [1] = true}'), ('set_false', 'set<int>', 'return {[3] = false}'), ('set_one', 'set<int>', 'return {[3] = 1}'),
    ('set_seq', 'set<int>', 'return {3, 1}'), ('set_text', 'set<text>', 'return {b = true, a = true}'), ('set_empty', 'set<text>', 'return {}'),
    ('set_bad_key', 'set<int>', 'return {x = true}'),
    ('map_ok', 'map<text, int>', 'return {b = 2, a = 1}'), ('map_seq', 'map<int, text>', 'return {"x", "y"}'), ('map_bad_key', 'map<int, text>', 'return {x = "b"}'),
    ('map_table_val', 'map<int, text>', 'return {[1] = {}}'), ('map_table_key', 'map<text, int>', 'return {[{}] = 1}'), ('map_bool_key', 'map<text, int>', 'return {[true] = 1}'),
    ('map_empty', 'map<text, int>', 'return {}'), ('map_nested', 'map<int, frozen<list<text>>>', 'return {[2] = {"a"}, [1] = {}}'),
    ('tuple_ok', 'tuple<int, text>', 'return {1, "a"}'), ('tuple_short', 'tuple<int, text>', 'return {1}'), ('tuple_long', 'tuple<int, text>', 'return {1, "a", 3}'),
    ('tuple_gap', 'tuple<int, text>', 'return {[1] = 1, [3] = "a"}'), ('tuple_named', 'tuple<int, text>', 'return {x = 1}'), ('tuple_bad_elem', 'tuple<int, text>', 'return {"a", 1}'),
    ('tuple_int', 'tuple<int, text>', 'return 1'), ('tuple_hole', 'tuple<int, text>', 'return {[2] = "a"}'), ('tuple_empty', 'tuple<int, text>', 'return {}'),
    ('udt_ok', 'u', 'return {a = 1, b = "x"}'), ('udt_missing', 'u', 'return {a = 1}'), ('udt_extra', 'u', 'return {a = 1, b = "x", c = 2}'),
    ('udt_int_key', 'u', 'return {a = 1, b = "x", [1] = 2}'), ('udt_bad_field', 'u', 'return {a = "x", b = "y"}'), ('udt_empty', 'u', 'return {}'),
    ('vector_ok', 'vector<float, 2>', 'return {1, 2}'), ('vector_short', 'vector<float, 2>', 'return {1}'), ('vector_long', 'vector<float, 2>', 'return {1, 2, 3}'),
    ('vector_gap', 'vector<float, 2>', 'return {[1] = 1, [3] = 2}'), ('vector_str', 'vector<float, 2>', 'return {1.5, "2"}'), ('vector_int', 'vector<float, 2>', 'return 1'),
]

# (id, arg type, return type, body) using decimal arguments.
DECIMAL_OPS = [
    ('dec_mt', 'varint', 'text', 'local t = {} for k, v in pairs(getmetatable(x)) do t[#t+1] = k .. ":" .. type(v) end table.sort(t) return table.concat(t, " ")'),
    ('dec_index', 'varint', 'text', 'return tostring(getmetatable(x).__index == getmetatable(x))'),
    ('dec_name', 'decimal', 'text', 'return type(x) .. " " .. getmetatable(x).__name'),
    ('vi_add_int', 'varint', 'varint', 'return x + 1'), ('vi_add_self', 'varint', 'varint', 'return x + x'), ('vi_radd', 'varint', 'varint', 'return 1 + x'),
    ('vi_add_float', 'varint', 'text', 'return x + 1.5'), ('vi_add_float_type', 'varint', 'text', 'return type(x + 1.5)'),
    ('vi_sub', 'varint', 'varint', 'return x - 1'), ('vi_add_str', 'varint', 'text', 'return x + "1.5"'), ('vi_sub_str', 'varint', 'text', 'return x - "2.5"'),
    ('vi_rsub_float', 'varint', 'text', 'return 2.5 - x'), ('vi_add_bad_str', 'varint', 'text', 'return x + "abc"'), ('vi_add_table', 'varint', 'text', 'return x + {}'),
    ('vi_add_bool', 'varint', 'text', 'return x + true'), ('vi_mul', 'varint', 'text', 'return x * 2'), ('vi_div', 'varint', 'text', 'return x / 2'),
    ('vi_idiv', 'varint', 'text', 'return x // 2'), ('vi_mod', 'varint', 'text', 'return x % 2'), ('vi_unm', 'varint', 'text', 'return -x'),
    ('vi_pow', 'varint', 'text', 'return x ^ 2'), ('vi_eq', 'varint', 'text', 'return tostring(x == x) .. tostring(x == x + 0)'),
    ('vi_lt', 'varint', 'text', 'return tostring(x < x)'), ('vi_concat', 'varint', 'text', 'return x .. ""'), ('vi_len', 'varint', 'text', 'return #x'),
    ('vi_band', 'varint', 'text', 'return x & 1'), ('vi_to_bigint', 'varint', 'bigint', 'return x'), ('vi_to_int', 'varint', 'int', 'return x'),
    ('vi_to_double', 'varint', 'double', 'return x'), ('vi_to_float', 'varint', 'float', 'return x'), ('vi_to_decimal', 'varint', 'decimal', 'return x'),
    ('vi_to_text', 'varint', 'text', 'return x'), ('vi_to_ascii', 'varint', 'ascii', 'return x'), ('vi_to_blob', 'varint', 'blob', 'return x'),
    ('vi_to_bool', 'varint', 'boolean', 'return x'), ('vi_to_ts', 'varint', 'timestamp', 'return x'), ('vi_to_date', 'varint', 'date', 'return x'),
    ('vi_to_time', 'varint', 'time', 'return x'), ('vi_to_list', 'varint', 'list<int>', 'return {x, x + 1}'),
    ('de_add_int', 'decimal', 'decimal', 'return x + 1'), ('de_add_zero', 'decimal', 'decimal', 'return x + 0'), ('de_sub_self', 'decimal', 'decimal', 'return x - x'),
    ('de_sub_float', 'decimal', 'decimal', 'return x - 0.5'), ('de_sub_self_text', 'decimal', 'text', 'return x - x'),
    ('de_add_scale', 'decimal', 'text', 'return x - x + "2.50"'), ('de_add_exp', 'decimal', 'text', 'return x - x + "1e3"'), ('de_to_int', 'decimal', 'int', 'return x'),
    ('de_to_varint', 'decimal', 'varint', 'return x'), ('de_to_double', 'decimal', 'double', 'return x'), ('de_to_text', 'decimal', 'text', 'return x'),
    ('de_to_blob', 'decimal', 'blob', 'return x'), ('de_integral_to_int', 'decimal', 'int', 'return x - x + "2.0"'),
    ('de_to_ts', 'decimal', 'timestamp', 'return x'),
]


def lua_returns():
    k = 'udfv_ret'
    setup = [ks(k), 'CREATE TYPE %s.u (a int, b text)' % k, 'CREATE TABLE %s.t (pk int PRIMARY KEY, vi varint, de decimal, de2 decimal)' % k,
             "INSERT INTO %s.t (pk, vi, de, de2) VALUES (1, 123456789012345678901234567890, 12345678901234567890.123456789, 1.5E+3)" % k]
    cases = []
    for cid, ty, body in RETURNS:
        cases.append((cid + '_fn', "CREATE FUNCTION %s.%s() CALLED ON NULL INPUT RETURNS %s LANGUAGE lua AS '%s'" % (k, cid, ty, body.replace("'", "''"))))
        cases.append((cid, 'SELECT %s.%s() FROM %s.t' % (k, cid, k)))
    for cid, aty, ty, body in DECIMAL_OPS:
        col = 'vi' if aty == 'varint' else 'de'
        cases.append((cid + '_fn', "CREATE FUNCTION %s.%s(x %s) CALLED ON NULL INPUT RETURNS %s LANGUAGE lua AS '%s'" % (k, cid, aty, ty, body)))
        cases.append((cid, 'SELECT %s.%s(%s) FROM %s.t' % (k, cid, col, k)))
    cases.append(('de_neg_scale_same', 'SELECT %s.de_to_text(de2), %s.de_add_zero(de2) FROM %s.t' % (k, k, k)))
    cases.append(('de_neg_scale_ret_fn', "CREATE FUNCTION %s.de_ret(x decimal) CALLED ON NULL INPUT RETURNS decimal LANGUAGE lua AS 'return x'" % k))
    cases.append(('de_neg_scale_ret', 'SELECT %s.de_ret(de2) FROM %s.t' % (k, k)))
    return ('lua_returns', {'scylla'}, setup, cases)


# (id, return type, body) of runtime behaviour, errors and limits.
RUNTIME = [
    ('version', 'text', 'return _VERSION'),
    ('globals', 'text', 'local t = {} for k, v in pairs(_G) do t[#t+1] = k .. ":" .. type(v) end table.sort(t) return table.concat(t, " ")'),
    ('string_lib', 'text', 'local t = {} for k in pairs(string) do t[#t+1] = k end table.sort(t) return table.concat(t, " ")'),
    ('table_lib', 'text', 'local t = {} for k in pairs(table) do t[#t+1] = k end table.sort(t) return table.concat(t, " ")'),
    ('coroutine_lib', 'text', 'local t = {} for k in pairs(coroutine) do t[#t+1] = k end table.sort(t) return table.concat(t, " ")'),
    ('no_math', 'text', 'return math.floor(1.5)'), ('no_os', 'text', 'return tostring(os) .. tostring(io) .. tostring(debug) .. tostring(utf8) .. tostring(require) .. tostring(package)'),
    ('error_str', 'int', 'error("boom")'), ('error_level0', 'int', 'error("boom", 0)'), ('error_level2', 'int', 'error("boom", 2)'),
    ('error_num', 'int', 'error(42)'), ('error_float', 'int', 'error(1.5)'), ('assert_false', 'int', 'assert(false)'), ('assert_msg', 'int', 'assert(false, "m")'),
    ('assert_num', 'int', 'assert(nil, 5)'), ('index_nil', 'int', 'local t = nil return t.x'), ('index_global', 'int', 'return nope.x'),
    ('call_nil', 'int', 'return nope()'), ('arith_nil', 'int', 'return nope + 1'), ('arith_str', 'int', 'return "a" + 1'),
    ('concat_table', 'text', 'return "a" .. {}'), ('compare', 'int', 'return tostring(1 < "x")'), ('idiv_zero', 'int', 'return 1 // 0'),
    ('mod_zero', 'int', 'return 1 % 0'), ('format_frac', 'text', 'return string.format("%d", 1.5)'), ('concat_bad', 'text', 'return table.concat({{}})'),
    ('rep_neg', 'text', 'return string.rep("x", -1)'), ('bad_arg', 'text', 'return ("x"):rep({})'), ('setmt_bad', 'text', 'return setmetatable(1, {})'),
    ('pcall_error', 'text', 'local ok, e = pcall(error, "in") return tostring(ok) .. e'),
    ('pcall_index', 'text', 'local ok, e = pcall(function() local t = nil return t.x end) return tostring(ok) .. e'),
    ('pcall_nil_error', 'text', 'local ok, e = pcall(error) return tostring(ok) .. tostring(e)'),
    ('pcall_table_error', 'text', 'local ok, e = pcall(error, {}) return tostring(ok) .. type(e)'),
    ('xpcall_handler', 'text', 'return select(2, xpcall(error, function(m) return "h:" .. tostring(m) end, "z"))'),
    ('pcall_memory', 'text', 'local ok, e = pcall(string.rep, "x", 2000000) return tostring(ok) .. tostring(e)'),
    ('timeout', 'int', 'while true do end'), ('timeout_pcall', 'int', 'while true do pcall(function() while true do end end) end'),
    ('timeout_coroutine', 'int', 'local co = coroutine.wrap(function() while true do end end) while true do co() end'),
    ('memory_table', 'int', 'local t = {} for i = 1, 100000000 do t[i] = i end return 1'),
    ('memory_string', 'text', 'return string.rep("x", 2000000)'), ('memory_small', 'text', 'return tostring(#string.rep("x", 100000))'),
    ('recursion', 'text', 'local function f(n) return f(n + 1) + 1 end return f(1)'),
    ('recursion_ok', 'text', 'local function f(n) if n == 0 then return 0 end return f(n - 1) + 1 end return tostring(f(100))'),
    ('load_text', 'text', 'return tostring(load("return 1+1")())'), ('load_env', 'text', 'return tostring(load("return x", "c", "t", {x = 5})())'),
    ('load_binary', 'text', 'return tostring(load(string.dump(function() return 1 end))())'),
    ('load_syntax', 'text', 'return tostring(select(2, load("return (")))'),
    ('print', 'text', 'print("hi") return "p"'), ('warn', 'text', 'warn("w") return "w"'), ('collect', 'text', 'return type(collectgarbage("count"))'),
    ('dofile_missing', 'text', 'return dofile("/nope")'), ('loadfile_missing', 'text', 'return tostring(select(2, loadfile("/nope")))'),
    ('globals_fresh', 'text', 'X = (X or 0) + 1 return tostring(X)'),
    ('tostrings', 'text', 'return tostring(0.1 + 0.2) .. " " .. tostring(2^63) .. " " .. tostring(9007199254740993) .. " " .. tostring(2.0) .. " " .. tostring(7 // 2.0) .. " " .. tostring(5 | 3) .. " " .. tostring(3 / 1) .. " " .. string.format("%d %5.2f %q", 3, 3.14159, 1.5)'),
    ('integer_ops', 'text', 'return tostring(7 // 2) .. " " .. tostring(-7 // 2) .. " " .. tostring(7 % -3) .. " " .. tostring(1 << 62) .. " " .. tostring(~0) .. " " .. tostring(9223372036854775807 + 1) .. " " .. tostring(math == nil)'),
    ('tonumber', 'text', 'return tostring(tonumber("0x10")) .. " " .. tostring(tonumber("1e2")) .. " " .. tostring(tonumber("z", 36)) .. " " .. tostring(tonumber("9223372036854775808"))'),
    ('coroutine_yield', 'text', 'return coroutine.wrap(function() coroutine.yield("y") end)()'),
    ('top_yield', 'text', 'coroutine.yield(1) return "a"'),
    ('string_methods', 'text', 'return ("abc"):upper() .. string.format("%s", 1) .. ("x"):rep(3, ",") .. select("#", string.byte("abc", 1, -1))'),
    ('string_pack', 'text', 'return string.unpack("<i4", string.pack("<i4", 7)) .. ""'),
    ('table_ops', 'text', 'local t = {3, 1, 2} table.sort(t) table.insert(t, 4) return table.concat(t, ",") .. " " .. select("#", table.unpack(t))'),
    ('goto_ok', 'text', 'local i = 0 ::top:: i = i + 1 if i < 3 then goto top end return tostring(i)'),
    ('close_var', 'text', 'local x <const> = 5 return tostring(x)'),
    ('int_for', 'text', 'local s = 0 for i = 1, 10 do s = s + i end return tostring(s)'),
    ('float_for', 'text', 'local s = 0 for i = 1, 2, 0.5 do s = s + i end return tostring(s)'),
    ('rawlen', 'text', 'return tostring(rawlen({1, 2})) .. tostring(rawequal("a", "a")) .. tostring(next({}))'),
    ('getmetatable_string', 'text', 'return tostring(getmetatable("").__index == string)'),
]

COMPILE = [
    ('syntax_eof', '', 'return ('), ('syntax_line2', '', 'local a = 1\nreturn ('), ('syntax_expr', 'x int', 'x + 1'),
    ('syntax_args_line2', 'a int, b int', 'a + '), ('syntax_end', '', 'end'), ('syntax_after_return', '', 'return 1 end return 2'),
    ('empty_body', '', ''), ('empty_body_args', 'x int', ''), ('comment_body', '', '-- nothing'), ('goto_missing', '', 'goto nowhere'),
    ('bad_escape', '', 'return "\\q"'), ('keyword_arg', '"end" int', 'return 1'), ('space_arg', '"a b" int', 'return 1'),
    ('quoted_arg', '"Ab" int', 'return Ab'), ('dup_args', 'a int, a int', 'return a'), ('long_string', '', 'return [[x'),
    ('unfinished_string', '', 'return "x'), ('number_bad', '', 'return 1..2'), ('assign_bad', '', 'x ='), ('const_assign', '', 'local x <const> = 1 x = 2'),
    ('break_outside', '', 'break'), ('vararg_bad', '', 'local function f() return ... end'),
]


def lua_runtime():
    k = 'udfv_run'
    setup = [ks(k), 'CREATE TABLE %s.t (pk int PRIMARY KEY, v int)' % k, 'INSERT INTO %s.t (pk, v) VALUES (1, 1)' % k]
    cases = []
    for cid, ty, body in RUNTIME:
        cases.append((cid + '_fn', "CREATE FUNCTION %s.%s() CALLED ON NULL INPUT RETURNS %s LANGUAGE lua AS '%s'" % (k, cid, ty, body)))
        cases.append((cid, 'SELECT %s.%s() FROM %s.t' % (k, cid, k)))
        if cid == 'globals_fresh':
            cases.append((cid + '_again', 'SELECT %s.%s() FROM %s.t' % (k, cid, k)))
    for cid, args, body in COMPILE:
        cases.append((cid, "CREATE FUNCTION %s.%s(%s) CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS '%s'" % (k, cid, args, body)))
        if args:
            vals = ', '.join(['v'] * len(args.split(',')))
        else:
            vals = ''
        cases.append((cid + '_call', 'SELECT %s.%s(%s) FROM %s.t' % (k, cid, vals, k)))
    return ('lua_runtime', {'scylla'}, setup, cases)


def lua_ddl():
    k = 'udfv_ddl'
    setup = [ks(k), 'CREATE TYPE %s.u (a int)' % k, 'CREATE TABLE %s.t (pk int PRIMARY KEY, v int)' % k, 'INSERT INTO %s.t (pk, v) VALUES (1, 1)' % k]
    C = []
    def c(cid, q): C.append((cid, q))
    f = "CREATE FUNCTION %s.f1(a int, b int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return a + b'" % k
    c('create', f)
    c('create_exists', f)
    c('create_if_not_exists', f.replace('CREATE FUNCTION', 'CREATE FUNCTION IF NOT EXISTS'))
    c('create_or_replace_if_not_exists', f.replace('CREATE FUNCTION', 'CREATE OR REPLACE FUNCTION IF NOT EXISTS'))
    c('replace_null_directive', f.replace('CREATE FUNCTION', 'CREATE OR REPLACE FUNCTION').replace('RETURNS NULL ON NULL INPUT', 'CALLED ON NULL INPUT'))
    c('replace_return_text', f.replace('CREATE FUNCTION', 'CREATE OR REPLACE FUNCTION').replace('RETURNS int', 'RETURNS text'))
    c('replace_return_bigint', f.replace('CREATE FUNCTION', 'CREATE OR REPLACE FUNCTION').replace('RETURNS int', 'RETURNS bigint'))
    c('call_replaced', 'SELECT %s.f1(v, v) FROM %s.t' % (k, k))
    c('rows_f1', "SELECT function_name, argument_types, argument_names, body, called_on_null_input, language, return_type FROM system_schema.functions WHERE keyspace_name = '%s'" % k)
    for cid, lang, body in [('java', 'java', 'return a;'), ('wasm', 'wasm', '(module)'), ('xyz', 'xyz', 'x'), ('upper_lua', 'LUA', 'return a'), ('mixed_lua', 'Lua', 'return a')]:
        c('language_' + cid, "CREATE FUNCTION %s.l_%s(a int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE %s AS '%s'" % (k, cid, lang, body))
    def fn(name, args, ret, body='return 1'):
        return "CREATE FUNCTION %s.%s(%s) RETURNS NULL ON NULL INPUT RETURNS %s LANGUAGE lua AS '%s'" % (k, name, args, ret, body)
    c('dup_args', fn('fd', 'a int, a int', 'int'))
    c('frozen_list_arg', fn('fz1', 'a frozen<list<int>>', 'int'))
    c('frozen_set_ret', fn('fz2', 'a int', 'frozen<set<int>>'))
    c('frozen_tuple', fn('fz3', 'a tuple<int, int>', 'frozen<tuple<int>>'))
    c('frozen_udt_arg', fn('fz4', 'a frozen<u>', 'int'))
    c('frozen_udt_ret', fn('fz5', 'a int', 'frozen<u>'))
    c('udt_arg', fn('fz6', 'a u', 'u', 'return a'))
    c('no_keyspace', fn('fn', 'a int', 'int').replace(k + '.', 'udfv_nope.'))
    c('no_keyspace_specified', fn('fn', 'a int', 'int').replace(k + '.', ''))
    c('unknown_arg_type', fn('fn', 'a nosuchtype', 'int'))
    c('unknown_ret_type', fn('fn', 'a int', 'nosuchtype'))
    c('counter_arg', fn('fc1', 'a counter', 'int'))
    c('counter_ret', fn('fc2', 'a int', 'counter'))
    c('duration_arg', fn('fdu', 'a duration', 'duration', 'return a'))
    c('bracket_name', fn('"f[1]"', 'a int', 'int'))
    c('slash_name', fn('"f/1"', 'a int', 'int'))
    c('native_name_token', fn('token', 'a int', 'int'))
    c('native_name_count', fn('count', 'a int', 'int'))
    c('native_name_now', fn('now', '', 'int'))
    c('system_ks', fn('fs', 'a int', 'int').replace(k + '.', 'system.'))
    c('system_schema_ks', fn('fs', 'a int', 'int').replace(k + '.', 'system_schema.'))
    c('call_token_udf', 'SELECT %s.token(v) FROM %s.t' % (k, k))
    c('call_wrong_types', "SELECT %s.f1(v, 'x') FROM %s.t" % (k, k))
    c('call_wrong_arity', 'SELECT %s.f1(v) FROM %s.t' % (k, k))
    c('call_unknown', 'SELECT %s.nope(v) FROM %s.t' % (k, k))
    c('call_in_where', 'SELECT pk FROM %s.t WHERE pk = %s.f1(0, 1)' % (k, k))
    c('call_in_insert', 'INSERT INTO %s.t (pk, v) VALUES (%s.f1(1, 1), %s.f1(2, 2))' % (k, k, k))
    c('read_insert', 'SELECT * FROM %s.t WHERE pk = 2' % k)
    c('call_in_update', 'UPDATE %s.t SET v = %s.f1(3, 3) WHERE pk = 1' % (k, k))
    c('read_update', 'SELECT * FROM %s.t WHERE pk = 1' % k)
    c('drop_type_used', 'DROP TYPE %s.u' % k)
    # overloads and drops
    c('overload', fn('f1', 'a text, b text', 'text', 'return a .. b'))
    c('drop_ambiguous', 'DROP FUNCTION %s.f1' % k)
    c('drop_missing_overload', 'DROP FUNCTION %s.f1(int)' % k)
    c('drop_missing_overload_type', 'DROP FUNCTION %s.f1(nosuchtype)' % k)
    c('drop_missing', 'DROP FUNCTION %s.nope' % k)
    c('drop_missing_if_exists', 'DROP FUNCTION IF EXISTS %s.nope' % k)
    c('drop_no_keyspace', 'DROP FUNCTION udfv_nope.nope')
    c('drop_no_keyspace_if_exists', 'DROP FUNCTION IF EXISTS udfv_nope.nope')
    c('drop_no_keyspace_specified', 'DROP FUNCTION nope')
    c('drop_native', 'DROP FUNCTION system.now')
    c('drop_native_aggregate', 'DROP AGGREGATE system.count')
    c('drop_overload', 'DROP FUNCTION %s.f1(text, text)' % k)
    c('drop_frozen_arg', 'DROP FUNCTION %s.fz1(frozen<list<int>>)' % k)
    c('drop_unfrozen_arg', 'DROP FUNCTION %s.fz3(tuple<int, int>)' % k)
    c('drop_last', 'DROP FUNCTION %s.f1' % k)
    c('drop_again', 'DROP FUNCTION %s.f1' % k)
    return ('lua_ddl', {'scylla'}, setup, C)


def lua_aggregates():
    k = 'udfv_agg'
    setup = [ks(k), 'CREATE TABLE %s.t (pk int, ck int, v int, PRIMARY KEY (pk, ck))' % k,
             'INSERT INTO %s.t (pk, ck, v) VALUES (1, 1, 10)' % k, 'INSERT INTO %s.t (pk, ck, v) VALUES (1, 2, 20)' % k,
             'INSERT INTO %s.t (pk, ck) VALUES (1, 3)' % k, 'INSERT INTO %s.t (pk, ck, v) VALUES (3, 1, 5)' % k,
             "CREATE TABLE %s.e (pk int PRIMARY KEY, v int)" % k]
    C = []
    def c(cid, q): C.append((cid, q))
    def fn(name, args, ret, body, called=False):
        return "CREATE FUNCTION %s.%s(%s) %s RETURNS %s LANGUAGE lua AS '%s'" % (
            k, name, args, 'CALLED ON NULL INPUT' if called else 'RETURNS NULL ON NULL INPUT', ret, body)
    c('acc', fn('acc', 's int, v int', 'int', 'return (s or 100) + (v or 1000)', True))
    c('accr', fn('accr', 's int, v int', 'int', 'return s + v'))
    c('fin', fn('fin', 's int', 'text', 'return "r" .. s'))
    c('finc', fn('finc', 's int', 'text', 'return "r" .. tostring(s)', True))
    c('red', fn('red', 'a int, b int', 'int', 'return a + b'))
    c('redt', fn('redt', 'a int, b int', 'text', 'return a + b'))
    c('st', fn('st', 's text, v int', 'int', 'return v'))
    c('boom', fn('boom', 's int, v int', 'int', 'error("agg")', True))
    def ag(name, rest):
        return 'CREATE AGGREGATE %s.%s(int) SFUNC %s' % (k, name, rest)
    c('a1', ag('a1', 'acc STYPE int'))
    c('a1_rows', 'SELECT %s.a1(v) FROM %s.t' % (k, k))
    c('a1_partition', 'SELECT %s.a1(v) FROM %s.t WHERE pk = 1' % (k, k))
    c('a1_empty', 'SELECT %s.a1(v) FROM %s.t WHERE pk = 2' % (k, k))
    c('a1_group', 'SELECT pk, %s.a1(v) FROM %s.t GROUP BY pk' % (k, k))
    c('a1_empty_table', 'SELECT %s.a1(v) FROM %s.e' % (k, k))
    c('a2', ag('a2', 'accr STYPE int'))
    c('a2_rows', 'SELECT %s.a2(v) FROM %s.t' % (k, k))
    c('a3', ag('a3', 'accr STYPE int INITCOND 0'))
    c('a3_rows', 'SELECT %s.a3(v) FROM %s.t' % (k, k))
    c('a3_no_nulls', 'SELECT %s.a3(v) FROM %s.t WHERE pk = 3' % (k, k))
    c('a3_empty', 'SELECT %s.a3(v) FROM %s.t WHERE pk = 2' % (k, k))
    c('a4', ag('a4', 'accr STYPE int FINALFUNC fin INITCOND 0'))
    c('a4_rows', 'SELECT %s.a4(v) FROM %s.t WHERE pk = 3' % (k, k))
    c('a4_empty', 'SELECT %s.a4(v) FROM %s.t WHERE pk = 2' % (k, k))
    c('a5', ag('a5', 'acc STYPE int FINALFUNC finc'))
    c('a5_rows', 'SELECT %s.a5(v) FROM %s.t' % (k, k))
    c('a5_empty', 'SELECT %s.a5(v) FROM %s.t WHERE pk = 2' % (k, k))
    c('a6', ag('a6', 'accr STYPE int REDUCEFUNC red INITCOND 0'))
    c('a6_rows', 'SELECT %s.a6(v), count(*) FROM %s.t WHERE pk = 3' % (k, k))
    c('a7', ag('a7', 'accr STYPE int REDUCEFUNC red FINALFUNC fin INITCOND 0'))
    c('a7_rows', 'SELECT %s.a7(v) FROM %s.t WHERE pk = 3' % (k, k))
    c('a7_order', ag('a7b', 'accr STYPE int FINALFUNC fin REDUCEFUNC red INITCOND 0'))
    c('a7_initcond_first', ag('a7c', 'accr STYPE int INITCOND 0 REDUCEFUNC red'))
    c('a7_initcond_final', ag('a7d', 'accr STYPE int INITCOND 5 FINALFUNC fin'))
    c('a_boom', ag('aboom', 'boom STYPE int'))
    c('a_boom_rows', 'SELECT %s.aboom(v) FROM %s.t' % (k, k))
    c('exists', ag('a1', 'acc STYPE int'))
    c('if_not_exists', ag('a1', 'acc STYPE int').replace('CREATE AGGREGATE', 'CREATE AGGREGATE IF NOT EXISTS'))
    c('or_replace_if_not_exists', ag('a1', 'acc STYPE int').replace('CREATE AGGREGATE', 'CREATE OR REPLACE AGGREGATE IF NOT EXISTS'))
    c('replace_final', ag('a3', 'accr STYPE int FINALFUNC fin INITCOND 0').replace('CREATE AGGREGATE', 'CREATE OR REPLACE AGGREGATE'))
    c('replaced_rows', 'SELECT %s.a3(v) FROM %s.t WHERE pk = 3' % (k, k))
    c('state_missing', ag('b1', 'nope STYPE int'))
    c('final_missing', ag('b1', 'acc STYPE int FINALFUNC nope'))
    c('reduce_missing', ag('b1', 'acc STYPE int REDUCEFUNC nope'))
    c('reduce_text', ag('b2', 'acc STYPE int REDUCEFUNC redt'))
    c('reduce_unary', ag('b1', 'accr STYPE int REDUCEFUNC fin INITCOND 1'))
    c('reduce_state', ag('b3', 'accr STYPE int REDUCEFUNC accr INITCOND 1'))
    c('stype_mismatch', ag('b1', 'acc STYPE text'))
    c('state_returns_other', ag('b1', 'st STYPE text INITCOND \'a\''))
    c('final_wrong_arity', ag('b1', 'accr STYPE int FINALFUNC accr INITCOND 1'))
    c('state_is_aggregate', ag('b1', 'a1 STYPE int'))
    c('final_is_aggregate', ag('b1', 'acc STYPE int FINALFUNC a1'))
    c('frozen_stype', ag('b1', 'acc STYPE frozen<list<int>>'))
    c('frozen_arg', 'CREATE AGGREGATE %s.b1(frozen<list<int>>) SFUNC acc STYPE int' % k)
    c('arg_text', 'CREATE AGGREGATE %s.b1(text) SFUNC accr STYPE int INITCOND 1' % k)
    c('over_function', 'CREATE AGGREGATE %s.red(int, int) SFUNC acc STYPE int' % k)
    c('function_over_aggregate', fn('a1', 'a int', 'int', 'return a'))
    c('initcond_str', ag('b1', "accr STYPE int INITCOND 'x'"))
    c('initcond_list', ag('b1', 'accr STYPE int INITCOND [1]'))
    c('initcond_range', ag('b1', 'accr STYPE int INITCOND 99999999999'))
    c('initcond_float', ag('b1', 'accr STYPE int INITCOND 1.5'))
    c('initcond_tuple', ag('b1', 'accr STYPE int INITCOND (1, 2)'))
    c('initcond_null', ag('b4', 'acc STYPE int INITCOND null'))
    c('initcond_neg', ag('b5', 'accr STYPE int INITCOND -1'))
    c('system_ks', 'CREATE AGGREGATE system.b1(int) SFUNC acc STYPE int')
    c('no_keyspace', 'CREATE AGGREGATE udfv_nope.b1(int) SFUNC acc STYPE int')
    c('no_keyspace_specified', 'CREATE AGGREGATE b1(int) SFUNC acc STYPE int')
    c('rows_aggregates', "SELECT aggregate_name, argument_types, final_func, initcond, return_type, state_func, state_type FROM system_schema.aggregates WHERE keyspace_name = '%s'" % k)
    c('rows_scylla_aggregates', "SELECT * FROM system_schema.scylla_aggregates WHERE keyspace_name = '%s'" % k)
    c('drop_used', 'DROP FUNCTION %s.accr' % k)
    c('drop_used_final', 'DROP FUNCTION %s.fin' % k)
    c('drop_aggregate_function', 'DROP AGGREGATE %s.red' % k)
    c('drop_function_aggregate', 'DROP FUNCTION %s.a1' % k)
    c('drop_aggregate_missing_overload', 'DROP AGGREGATE %s.a1(text)' % k)
    c('drop_aggregate_missing', 'DROP AGGREGATE %s.nope' % k)
    c('drop_aggregate', 'DROP AGGREGATE %s.a5' % k)
    c('drop_aggregate_if_exists', 'DROP AGGREGATE IF EXISTS %s.a5' % k)
    c('drop_final_after', 'DROP FUNCTION %s.finc' % k)
    c('uda_in_where', 'SELECT pk FROM %s.t WHERE v = %s.a1(1) ALLOW FILTERING' % (k, k))
    return ('lua_aggregates', {'scylla'}, setup, C)


# ---- Cassandra (Java) ------------------------------------------------------------------------

def java_functions():
    k = 'udfv_java'
    setup = [ks(k), 'CREATE TYPE %s.u (a int)' % k, 'CREATE TABLE %s.t (pk int PRIMARY KEY, v int, s text)' % k,
             "INSERT INTO %s.t (pk, v, s) VALUES (1, 1, 'a')" % k, 'INSERT INTO %s.t (pk) VALUES (2)' % k]
    C = []
    def c(cid, q): C.append((cid, q))
    def fn(name, args, ret, body, called=False, lang='java'):
        return "CREATE FUNCTION %s.%s(%s) %s RETURNS %s LANGUAGE %s AS '%s'" % (
            k, name, args, 'CALLED ON NULL INPUT' if called else 'RETURNS NULL ON NULL INPUT', ret, lang, body)
    c('create', fn('f1', 'a int', 'int', 'return a + 1;'))
    c('call', 'SELECT pk, %s.f1(v) FROM %s.t' % (k, k))
    c('exists', fn('f1', 'a int', 'int', 'return a + 1;'))
    c('if_not_exists', fn('f1', 'a int', 'int', 'return a + 1;').replace('CREATE FUNCTION', 'CREATE FUNCTION IF NOT EXISTS'))
    c('or_replace_if_not_exists', fn('f1', 'a int', 'int', 'return a + 1;').replace('CREATE FUNCTION', 'CREATE OR REPLACE FUNCTION IF NOT EXISTS'))
    c('replace_null_directive', fn('f1', 'a int', 'int', 'return a;', True).replace('CREATE FUNCTION', 'CREATE OR REPLACE FUNCTION'))
    c('replace_return_type', fn('f1', 'a int', 'text', 'return "x";').replace('CREATE FUNCTION', 'CREATE OR REPLACE FUNCTION'))
    c('replace_ok', fn('f1', 'a int', 'int', 'return a + 2;').replace('CREATE FUNCTION', 'CREATE OR REPLACE FUNCTION'))
    c('call_replaced', 'SELECT pk, %s.f1(v) FROM %s.t' % (k, k))
    c('throw', fn('f2', 'a int', 'int', 'throw new RuntimeException("boom");'))
    c('call_throw', 'SELECT %s.f2(v) FROM %s.t WHERE pk = 1' % (k, k))
    c('div_zero', fn('f3', 'a int', 'int', 'return 1 / (a - 1);'))
    c('call_div_zero', 'SELECT %s.f3(v) FROM %s.t WHERE pk = 1' % (k, k))
    c('called_null', fn('f4', 'a int', 'int', 'return a;', True))
    c('call_called_null', 'SELECT pk, %s.f4(v) FROM %s.t' % (k, k))
    c('called_null_npe', fn('f5', 'a int', 'int', 'return a + 1;', True))
    c('call_called_null_npe', 'SELECT %s.f5(v) FROM %s.t WHERE pk = 2' % (k, k))
    c('text_fn', fn('f6', 's text', 'text', 'return s.toUpperCase();'))
    c('call_text_fn', 'SELECT pk, %s.f6(s) FROM %s.t' % (k, k))
    c('rows_functions', "SELECT function_name, argument_types, argument_names, body, called_on_null_input, language, return_type FROM system_schema.functions WHERE keyspace_name = '%s'" % k)
    c('compile_error', fn('f7', 'a int', 'int', 'return a +;'))
    c('type_mismatch', fn('f8', 'a int', 'text', 'return 1;'))
    c('forbidden_class', fn('f9', 'a int', 'int', 'java.io.File f = new java.io.File("/"); return 1;'))
    c('return_null_primitive', fn('f10', 'a int', 'int', 'return null;'))
    c('missing_semicolon', fn('f11', 'a int', 'int', 'return a'))
    c('call_compile_error', 'SELECT %s.f7(v) FROM %s.t' % (k, k))
    c('language_lua', fn('l1', 'a int', 'int', 'return a', lang='lua'))
    c('language_javascript', fn('l2', 'a int', 'int', 'a', lang='javascript'))
    c('language_upper', fn('l3', 'a int', 'int', 'return a;', lang='JAVA'))
    c('dup_args', fn('d1', 'a int, a int', 'int', 'return a;'))
    c('frozen_arg', fn('d2', 'a frozen<list<int>>', 'int', 'return 1;'))
    c('frozen_ret', fn('d3', 'a int', 'frozen<set<int>>', 'return null;'))
    c('frozen_tuple', fn('d4', 'a tuple<int, int>', 'frozen<tuple<int>>', 'return null;'))
    c('frozen_udt', fn('d5', 'a frozen<u>', 'int', 'return 1;'))
    c('no_keyspace', fn('d6', 'a int', 'int', 'return a;').replace(k + '.', 'udfv_nope.'))
    c('no_keyspace_specified', fn('d6', 'a int', 'int', 'return a;').replace(k + '.', ''))
    c('unknown_type', fn('d6', 'a nosuchtype', 'int', 'return 1;'))
    c('counter_arg', fn('d7', 'a counter', 'bigint', 'return a;'))
    c('bracket_name', fn('"f[1]"', 'a int', 'int', 'return a;'))
    c('slash_name', fn('"f/1"', 'a int', 'int', 'return a;'))
    c('system_ks', fn('d8', 'a int', 'int', 'return a;').replace(k + '.', 'system.'))
    c('native_name_token', fn('token', 'a int', 'int', 'return a;'))
    c('call_wrong_types', "SELECT %s.f1(s) FROM %s.t" % (k, k))
    c('call_in_insert', 'INSERT INTO %s.t (pk, v) VALUES (%s.f1(5), 1)' % (k, k))
    c('drop_type_used', 'DROP TYPE %s.u' % k)
    c('overload', fn('f1', 'a text', 'text', 'return a;'))
    c('drop_ambiguous', 'DROP FUNCTION %s.f1' % k)
    c('drop_missing_overload', 'DROP FUNCTION %s.f1(bigint)' % k)
    c('drop_missing', 'DROP FUNCTION %s.nope' % k)
    c('drop_missing_if_exists', 'DROP FUNCTION IF EXISTS %s.nope' % k)
    c('drop_no_keyspace', 'DROP FUNCTION udfv_nope.nope')
    c('drop_no_keyspace_if_exists', 'DROP FUNCTION IF EXISTS udfv_nope.nope')
    c('drop_native', 'DROP FUNCTION system.now')
    c('drop_frozen_arg', 'DROP FUNCTION %s.d2(frozen<list<int>>)' % k)
    c('drop_overload', 'DROP FUNCTION %s.f1(text)' % k)
    return ('java_functions', {'cassandra'}, setup, C)


def java_aggregates():
    k = 'udfv_jagg'
    setup = [ks(k), 'CREATE TABLE %s.t (pk int, ck int, v int, PRIMARY KEY (pk, ck))' % k,
             'INSERT INTO %s.t (pk, ck, v) VALUES (1, 1, 10)' % k, 'INSERT INTO %s.t (pk, ck, v) VALUES (1, 2, 20)' % k,
             'INSERT INTO %s.t (pk, ck) VALUES (1, 3)' % k]
    C = []
    def c(cid, q): C.append((cid, q))
    def fn(name, args, ret, body, called=False):
        return "CREATE FUNCTION %s.%s(%s) %s RETURNS %s LANGUAGE java AS '%s'" % (
            k, name, args, 'CALLED ON NULL INPUT' if called else 'RETURNS NULL ON NULL INPUT', ret, body)
    c('acc', fn('acc', 's int, v int', 'int', 'return (s == null ? 100 : s) + (v == null ? 1000 : v);', True))
    c('accr', fn('accr', 's int, v int', 'int', 'return s + v;'))
    c('fin', fn('fin', 's int', 'text', 'return "r" + s;'))
    c('st', fn('st', 's text, v int', 'int', 'return v;'))
    def ag(name, rest):
        return 'CREATE AGGREGATE %s.%s(int) SFUNC %s' % (k, name, rest)
    c('a1', ag('a1', 'acc STYPE int'))
    c('a1_rows', 'SELECT %s.a1(v) FROM %s.t' % (k, k))
    c('a1_empty', 'SELECT %s.a1(v) FROM %s.t WHERE pk = 2' % (k, k))
    c('a2_no_initcond', ag('a2', 'accr STYPE int'))
    c('a3', ag('a3', 'accr STYPE int INITCOND 0'))
    c('a3_rows', 'SELECT %s.a3(v) FROM %s.t' % (k, k))
    c('a3_empty', 'SELECT %s.a3(v) FROM %s.t WHERE pk = 2' % (k, k))
    c('a4', ag('a4', 'accr STYPE int FINALFUNC fin INITCOND 0'))
    c('a4_rows', 'SELECT %s.a4(v) FROM %s.t' % (k, k))
    c('reducefunc', ag('a5', 'accr STYPE int REDUCEFUNC accr INITCOND 0'))
    c('exists', ag('a1', 'acc STYPE int'))
    c('if_not_exists', ag('a1', 'acc STYPE int').replace('CREATE AGGREGATE', 'CREATE AGGREGATE IF NOT EXISTS'))
    c('or_replace_if_not_exists', ag('a1', 'acc STYPE int').replace('CREATE AGGREGATE', 'CREATE OR REPLACE AGGREGATE IF NOT EXISTS'))
    c('replace_return_type', ag('a3', 'accr STYPE int FINALFUNC fin INITCOND 0').replace('CREATE AGGREGATE', 'CREATE OR REPLACE AGGREGATE'))
    c('state_missing', ag('b1', 'nope STYPE int'))
    c('final_missing', ag('b1', 'acc STYPE int FINALFUNC nope'))
    c('stype_mismatch', ag('b1', 'acc STYPE text'))
    c('state_returns_other', ag('b1', "st STYPE text INITCOND 'a'"))
    c('state_is_aggregate', ag('b1', 'a1 STYPE int'))
    c('final_is_aggregate', ag('b1', 'acc STYPE int FINALFUNC a1'))
    c('frozen_stype', ag('b1', 'acc STYPE frozen<list<int>>'))
    c('frozen_arg', 'CREATE AGGREGATE %s.b1(frozen<list<int>>) SFUNC acc STYPE int' % k)
    c('over_function', 'CREATE AGGREGATE %s.accr(int) SFUNC accr STYPE int INITCOND 0' % k)
    c('function_over_aggregate', fn('a1', 'a int', 'int', 'return a;'))
    c('initcond_str', ag('b1', "accr STYPE int INITCOND 'x'"))
    c('initcond_list', ag('b1', 'accr STYPE int INITCOND [1]'))
    c('initcond_range', ag('b1', 'accr STYPE int INITCOND 99999999999'))
    c('initcond_null', ag('b4', 'acc STYPE int INITCOND null'))
    c('initcond_bind', ag('b1', 'accr STYPE int INITCOND ?'))
    c('initcond_empty_text', 'CREATE AGGREGATE %s.b6(int) SFUNC st STYPE text INITCOND \'\'' % k)
    c('system_ks', 'CREATE AGGREGATE system.b1(int) SFUNC acc STYPE int')
    c('no_keyspace', 'CREATE AGGREGATE udfv_nope.b1(int) SFUNC acc STYPE int')
    c('rows_aggregates', "SELECT aggregate_name, argument_types, final_func, initcond, return_type, state_func, state_type FROM system_schema.aggregates WHERE keyspace_name = '%s'" % k)
    c('drop_used', 'DROP FUNCTION %s.accr' % k)
    c('drop_aggregate_function', 'DROP AGGREGATE %s.accr' % k)
    c('drop_function_aggregate', 'DROP FUNCTION %s.a1' % k)
    c('drop_aggregate_missing_overload', 'DROP AGGREGATE %s.a1(text)' % k)
    c('drop_aggregate_missing', 'DROP AGGREGATE %s.nope' % k)
    c('drop_aggregate', 'DROP AGGREGATE %s.a1' % k)
    return ('java_aggregates', {'cassandra'}, setup, C)


def cross():
    # The same statements on both dialects: languages each dialect rejects.
    k = 'udfv_x'
    setup = [ks(k), 'CREATE TABLE %s.t (pk int PRIMARY KEY, v int)' % k]
    C = [('lua_on', "CREATE FUNCTION %s.f(a int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return a'" % k),
         ('java_on', "CREATE FUNCTION %s.g(a int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return a;'" % k),
         ('or_replace_if_not_exists_aggregate', 'CREATE OR REPLACE AGGREGATE IF NOT EXISTS %s.a(int) SFUNC f STYPE int' % k),
         ('drop_aggregate_no_keyspace', 'DROP AGGREGATE udfv_nope.a'),
         ('drop_aggregate_if_exists_no_keyspace', 'DROP AGGREGATE IF EXISTS udfv_nope.a')]
    return ('cross', {'scylla', 'cassandra'}, setup, C)


def stored_types():
    # How argument and return types are stored, described and named in permissions.
    k = 'udfv_st'
    setup = [ks(k), 'CREATE TYPE %s.u (a int)' % k, 'CREATE TYPE %s."Weird[t]" (a int)' % k,
             'DROP ROLE IF EXISTS udfv_r', "CREATE ROLE udfv_r WITH PASSWORD = 'udfv_r' AND LOGIN = true"]
    C = []
    return k, setup, C


def stored_types_scylla():
    k, setup, C = stored_types()
    def c(cid, q): C.append((cid, q))
    c('f1', "CREATE FUNCTION %s.f1(a list<int>, b frozen<set<int>>, c map<int, text>, d tuple<uuid, inet>, e u, g frozen<list<frozen<u>>>, h vector<float, 2>) CALLED ON NULL INPUT RETURNS list<int> LANGUAGE lua AS 'return nil'" % k)
    c('f2', "CREATE FUNCTION %s.f2(a int) CALLED ON NULL INPUT RETURNS u LANGUAGE lua AS 'return nil'" % k)
    c('f3', "CREATE FUNCTION %s.f3(a int) CALLED ON NULL INPUT RETURNS tuple<int, text> LANGUAGE lua AS 'return nil'" % k)
    c('f4', "CREATE FUNCTION %s.f4(a int) CALLED ON NULL INPUT RETURNS frozen<map<int, int>> LANGUAGE lua AS 'return nil'" % k)
    c('weird', "CREATE FUNCTION %s.\"Odd[n]\"(c \"Weird[t]\") CALLED ON NULL INPUT RETURNS int LANGUAGE lua AS 'return 1'" % k)
    c('rows', "SELECT function_name, argument_types, return_type FROM system_schema.functions WHERE keyspace_name = '%s'" % k)
    for f in ('f1', 'f2', 'f3', 'f4', '"Odd[n]"'):
        c('describe_' + f.strip('"'), 'DESCRIBE FUNCTION %s.%s' % (k, f))
    c('grant_written', 'GRANT EXECUTE ON FUNCTION %s.f1(list<int>, frozen<set<int>>, map<int, text>, tuple<uuid, inet>, u, frozen<list<frozen<u>>>, vector<float, 2>) TO udfv_r' % k)
    c('grant_all_frozen', 'GRANT EXECUTE ON FUNCTION %s.f1(frozen<list<int>>, frozen<set<int>>, frozen<map<int, text>>, tuple<uuid, inet>, frozen<u>, frozen<list<frozen<u>>>, vector<float, 2>) TO udfv_r' % k)
    c('grant_weird', 'GRANT EXECUTE ON FUNCTION %s."Odd[n]"("Weird[t]") TO udfv_r' % k)
    c('list', 'LIST ALL PERMISSIONS OF udfv_r')
    c('drop_role', 'DROP ROLE udfv_r')
    return ('stored_types_scylla', {'scylla'}, setup, C)


def stored_types_cassandra():
    k, setup, C = stored_types()
    def c(cid, q): C.append((cid, q))
    c('f1', "CREATE FUNCTION %s.f1(a list<int>, c map<int, text>, d tuple<uuid, inet>, e u, h vector<float, 2>) CALLED ON NULL INPUT RETURNS list<int> LANGUAGE java AS 'return null;'" % k)
    c('f2', "CREATE FUNCTION %s.f2(a int) CALLED ON NULL INPUT RETURNS u LANGUAGE java AS 'return null;'" % k)
    c('f3', "CREATE FUNCTION %s.f3(a int) CALLED ON NULL INPUT RETURNS tuple<int, text> LANGUAGE java AS 'return null;'" % k)
    c('weird', "CREATE FUNCTION %s.\"Odd[n]\"(c \"Weird[t]\") CALLED ON NULL INPUT RETURNS int LANGUAGE java AS 'return 1;'" % k)
    c('rows', "SELECT function_name, argument_types, return_type FROM system_schema.functions WHERE keyspace_name = '%s'" % k)
    for f in ('f1', 'f2', 'f3'):
        c('describe_' + f, 'DESCRIBE FUNCTION %s.%s' % (k, f))
    c('grant_written', 'GRANT EXECUTE ON FUNCTION %s.f1(list<int>, map<int, text>, tuple<uuid, inet>, u, vector<float, 2>) TO udfv_r' % k)
    c('grant_frozen', 'GRANT EXECUTE ON FUNCTION %s.f1(frozen<list<int>>, map<int, text>, tuple<uuid, inet>, frozen<u>, vector<float, 2>) TO udfv_r' % k)
    c('list', 'LIST ALL PERMISSIONS OF udfv_r')
    c('drop_role', 'DROP ROLE udfv_r')
    return ('stored_types_cassandra', {'cassandra'}, setup, C)


def permissions(dialect):
    # UDF and UDA permissions as roles see them (udf_live.py logs in as role/role).
    lang, body, bbody, abody = ('lua', "'return a'", "'return b'", "'return (s or 0) + (v or 0)'") if dialect == 'scylla' else (
        'java', "'return a;'", "'return b;'", "'return (s == null ? 0 : s) + (v == null ? 0 : v);'")
    k, k2, r = 'udfv_perm', 'udfv_perm2', 'udfv_p1'
    setup = [ks(k), ks(k2), 'DROP ROLE IF EXISTS %s' % r, "CREATE ROLE %s WITH PASSWORD = '%s' AND LOGIN = true" % (r, r),
             'CREATE TABLE %s.t (pk int PRIMARY KEY, v int)' % k, 'INSERT INTO %s.t (pk, v) VALUES (1, 1)' % k]
    def fn(name, args, b=body, prefix='CREATE FUNCTION', where=k):
        return "%s %s.%s(%s) CALLED ON NULL INPUT RETURNS int LANGUAGE %s AS %s" % (prefix, where, name, args, lang, b)
    C = []
    def c(cid, q, who=None):
        C.append((cid, q) if who is None else (cid, q, who))
        if q.startswith(('GRANT', 'REVOKE')) and dialect == 'cassandra':
            C.append((cid + '_wait', '!sleep 2.5'))
    c('f1', fn('f', 'a int'))
    c('f2', fn('f', 'a int, b int'))
    c('h1', fn('h', 'a int'))
    c('h2', fn('h', 'a int', where=k2))
    c('acc', fn('acc', 's int, v int', abody))
    c('agg', 'CREATE AGGREGATE %s.ag(int) SFUNC acc STYPE int INITCOND 0' % k)
    c('select_no_execute', 'SELECT %s.f(v) FROM %s.t' % (k, k), r)
    c('grant_select', 'GRANT SELECT ON KEYSPACE %s TO %s' % (k, r))
    c('select_no_execute_2', 'SELECT %s.f(v) FROM %s.t' % (k, k), r)
    c('select_agg_no_execute', 'SELECT %s.ag(v) FROM %s.t' % (k, k), r)
    c('grant_execute_f', 'GRANT EXECUTE ON FUNCTION %s.f(int) TO %s' % (k, r))
    c('select_execute', 'SELECT %s.f(v) FROM %s.t' % (k, k), r)
    c('grant_execute_ag', 'GRANT EXECUTE ON FUNCTION %s.ag(int) TO %s' % (k, r))
    c('select_agg_no_sfunc', 'SELECT %s.ag(v) FROM %s.t' % (k, k), r)
    c('drop_no_permission', 'DROP FUNCTION %s.f(int)' % k, r)
    c('grant_drop_f1', 'GRANT DROP ON FUNCTION %s.f(int) TO %s' % (k, r))
    c('drop_other_overload', 'DROP FUNCTION %s.f(int, int)' % k, r)
    c('drop_ambiguous', 'DROP FUNCTION %s.f' % k, r)
    c('drop_granted', 'DROP FUNCTION %s.f(int)' % k, r)
    c('grant_drop_h', 'GRANT DROP ON FUNCTION %s.h(int) TO %s' % (k2, r))
    c('use_k', 'USE %s' % k, r)
    c('drop_unqualified_other_ks', 'DROP FUNCTION h(int)', r)
    c('drop_unqualified_other_ks_noargs', 'DROP FUNCTION h', r)
    c('drop_qualified_granted', 'DROP FUNCTION %s.h' % k2, r)
    c('create_no_permission', fn('g', 'a int'), r)
    c('grant_create', 'GRANT CREATE ON ALL FUNCTIONS IN KEYSPACE %s TO %s' % (k, r))
    c('create_granted', fn('g', 'a int'), r)
    c('replace_own', fn('g', 'a int', prefix='CREATE OR REPLACE FUNCTION'), r)
    c('replace_no_alter', fn('f', 'a int, b int', prefix='CREATE OR REPLACE FUNCTION'), r)
    c('create_if_not_exists_existing', fn('f', 'a int, b int', prefix='CREATE FUNCTION IF NOT EXISTS'), r)
    c('grant_alter_f2', 'GRANT ALTER ON FUNCTION %s.f(int, int) TO %s' % (k, r))
    c('replace_alter', fn('f', 'a int, b int', prefix='CREATE OR REPLACE FUNCTION'), r)
    c('create_agg_no_execute', 'CREATE AGGREGATE %s.ag2(int) SFUNC acc STYPE int INITCOND 0' % k, r)
    c('grant_execute_acc', 'GRANT EXECUTE ON FUNCTION %s.acc(int, int) TO %s' % (k, r))
    c('create_agg', 'CREATE AGGREGATE %s.ag2(int) SFUNC acc STYPE int INITCOND 0' % k, r)
    c('replace_agg_no_alter', 'CREATE OR REPLACE AGGREGATE %s.ag(int) SFUNC acc STYPE int INITCOND 1' % k, r)
    c('drop_agg_no_permission', 'DROP AGGREGATE %s.ag(int)' % k, r)
    c('drop_own_agg', 'DROP AGGREGATE %s.ag2(int)' % k, r)
    c('revoke_alter_f2', 'REVOKE ALTER ON FUNCTION %s.f(int, int) FROM %s' % (k, r))
    c('replace_revoked', fn('f', 'a int, b int', b=bbody, prefix='CREATE OR REPLACE FUNCTION'), r)
    c('grant_alter_all_in_ks', 'GRANT ALTER ON ALL FUNCTIONS IN KEYSPACE %s TO %s' % (k, r))
    c('replace_alter_all_in_ks', fn('f', 'a int, b int', b=bbody, prefix='CREATE OR REPLACE FUNCTION'), r)
    c('revoke_alter_all_in_ks', 'REVOKE ALTER ON ALL FUNCTIONS IN KEYSPACE %s FROM %s' % (k, r))
    c('replace_after_revoke_all', fn('f', 'a int, b int', prefix='CREATE OR REPLACE FUNCTION'), r)
    c('grant_alter_all', 'GRANT ALTER ON ALL FUNCTIONS TO %s' % r)
    c('replace_alter_all', fn('f', 'a int, b int', prefix='CREATE OR REPLACE FUNCTION'), r)
    c('revoke_alter_all', 'REVOKE ALTER ON ALL FUNCTIONS FROM %s' % r)
    c('replace_after_revoke_root', fn('f', 'a int, b int', b=bbody, prefix='CREATE OR REPLACE FUNCTION'), r)
    c('replace_agg_alter_revoked', 'CREATE OR REPLACE AGGREGATE %s.ag(int) SFUNC acc STYPE int INITCOND 2' % k, r)
    c('list', 'LIST ALL PERMISSIONS OF %s' % r)
    c('drop_role', 'DROP ROLE %s' % r)
    return ('permissions_' + dialect, {dialect}, setup, C)


BATTERIES = [lua_args(), lua_returns(), lua_runtime(), lua_ddl(), lua_aggregates(), java_functions(), java_aggregates(), cross(), stored_types_scylla(), stored_types_cassandra(), permissions('scylla'), permissions('cassandra')]
