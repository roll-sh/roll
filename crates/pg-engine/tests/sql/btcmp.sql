-- btree comparison support functions
SELECT bttextcmp('a','c'), bttextcmp('abc','ab'), bttextcmp('a','a'), bttextcmp('é','e'), bttext_pattern_cmp('a','z'), byteacmp('\x01','\x05'), byteacmp('\x0102','\x01');
SELECT bpcharcmp('a','z'), bpcharcmp('ab  ','abc'), bpcharcmp('ab','ab   '), btbpchar_pattern_cmp('ab ','abc'), btnamecmp('a','z'), btnamecmp('ab','abc'), btnamecmp('abc','ab'), btnametextcmp('ab','abc'), bttextnamecmp('a','z');
SELECT bitcmp(B'101', B'111'), varbitcmp(B'1', B'111'), bitcmp(B'1', B'10'), varbitcmp(B'10', B'1'), btint4cmp(1,5), btint4cmp(5,5), btint2cmp(9::int2, 1::int2), btint8cmp(1, 2), numeric_cmp(1,5), btfloat8cmp('NaN', 1), btfloat4cmp(1, 'NaN'), btint48cmp(5, 1), btint24cmp(1::int2, 7), btfloat48cmp(1, 2);
SELECT date_cmp('2020-01-02','2020-01-01'), date_cmp_timestamp('2020-01-01', '2020-01-01 00:00:01'), timestamp_cmp_timestamptz('2020-01-01', '2020-01-01'), timestamp_cmp('2020-01-01','2021-01-01'), timestamptz_cmp('2020-01-01','2021-01-01'), timestamptz_cmp_date('2020-01-01 01:00','2020-01-01'), time_cmp('01:00','09:00'), timetz_cmp('01:00+00','09:00+00'), interval_cmp('1 day','2 days'), interval_cmp('1 day','24 hours');
SELECT uuid_cmp('00000000-0000-0000-0000-000000000001','00000000-0000-0000-0000-000000000009'), uuid_cmp('00000000-0000-0000-0000-000000000001','00000000-0000-0000-0000-000000000001');
SELECT btarraycmp(ARRAY[1,2], ARRAY[1,3]), btarraycmp(ARRAY['a'],ARRAY['z']), btarraycmp(ARRAY[1],ARRAY[1,2]), btrecordcmp(ROW(1,'a'::text), ROW(1,'z'::text)), btrecordcmp(ROW('a'::text),ROW('z'::text));
SELECT macaddr_cmp('08:00:2b:01:02:03','08:00:2b:01:02:09'), macaddr8_cmp('08:00:2b:01:02:03:04:05','08:00:2b:01:02:03:04:09'), cash_cmp('1','9'), pg_lsn_cmp('0/1','0/9'), btcharcmp('a','z'), btoidvectorcmp('1 2','1 9'), bttidcmp('(0,1)','(0,9)'), btboolcmp(false,true), btoidcmp(1, 9);
SELECT network_cmp('10.0.0.1','10.0.0.9'), network_cmp('10.0.0.0/8','10.0.0.0/16'), network_cmp('::1','10.0.0.1'), network_cmp('10.0.0.1','::1'), network_cmp('10.0.0.0/7','11.0.0.0/7'), network_cmp('10.1.2.3/8','10.9.9.9/8'), network_cmp('10.0.0.1/32','10.0.0.1/32');
SELECT gin_cmp_tslexeme('abc','ab'), gin_cmp_tslexeme('a','z'), gin_cmp_tslexeme('a','a');
CREATE TYPE gx_bt_e AS ENUM ('a','b');
SELECT enum_cmp('a'::gx_bt_e,'b'::gx_bt_e), enum_cmp('b'::gx_bt_e,'b'::gx_bt_e);
DROP TYPE gx_bt_e;
SELECT bttextcmp(NULL, 'a'), btint4cmp(1, NULL);
-- jsonb order
SELECT jsonb_cmp('1','2'), jsonb_cmp('"a"','"z"'), jsonb_cmp('"ab"','"abc"'), jsonb_cmp('["a"]','["z"]'), jsonb_cmp('[1]','[1,2]'), jsonb_cmp('{"a":1}','{"a":2}'), jsonb_cmp('1','"a"'), jsonb_cmp('null','1');
SELECT jsonb_cmp('{"b":1}','{"aa":1}'), jsonb_cmp('{"a":1,"b":1}','{"a":1,"c":1}'), jsonb_cmp('[true]','[false]'), jsonb_cmp('[]','1'), jsonb_cmp('1', '[]'), jsonb_cmp('[[]]','[1]'), jsonb_cmp('[1]','1'), jsonb_cmp('{}','[]'), jsonb_cmp('[]','null'), jsonb_cmp('[1,2]','"zzz"'), jsonb_cmp('"zzz"','[1,2]'), jsonb_cmp('{}','1');
SELECT '{"b":1}'::jsonb < '{"aa":1}'::jsonb, '[[]]'::jsonb > '[1]'::jsonb, '[]'::jsonb < 'null'::jsonb;
SELECT j FROM (VALUES ('{"b":1}'::jsonb), ('{"aa":1}'), ('[]'), ('null'), ('1'), ('"x"'), ('[[]]'), ('[1]'), ('true'), ('{}'), ('[1,2]')) v(j) ORDER BY j;
SELECT p.oid::regprocedure, p.provolatile, p.proisstrict, p.prorettype::regtype FROM pg_proc p WHERE p.proname IN ('bttextcmp', 'btarraycmp', 'enum_cmp', 'network_cmp', 'jsonb_cmp', 'date_cmp_timestamp') ORDER BY p.oid;
-- hash support functions
SELECT hashint2(7::int2), hashint4(7), hashint8(7), hashint8(-7), hashoid(7), hashchar('x'), hashbool(true), hashbool(false), hashname('abc'), hashtext('abc'), hashbpchar('ab  '), hashbytea('\x0102');
SELECT hashfloat4(1.5), hashfloat8(1.5), hashfloat8(0), hashfloat8('-0'), hashfloat8('NaN'), hash_numeric(12.50), hash_numeric(0), hash_numeric('NaN'), hashdate('2020-01-01'), time_hash('01:02:03'), timetz_hash('01:02:03+02'), timestamp_hash('2020-01-01 01:02:03'), timestamptz_hash('2020-01-01 01:02:03+00');
SELECT interval_hash('1 mon 2 days 03:04:05'), interval_hash('30 days'), interval_hash('1 mon'), uuid_hash('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'), hashmacaddr('08:00:2b:01:02:03'), hashmacaddr8('08:00:2b:01:02:03:04:05'), hashinet('10.1.2.3/8'), hashinet('::1'), pg_lsn_hash('16/B374D848'), hashtid('(1,2)'), hashtid('(70000,3)'), hashoidvector('1 2 3');
SELECT jsonb_hash('null'), jsonb_hash('true'), jsonb_hash('"abc"'), jsonb_hash('12.5'), jsonb_hash('[1, "a", null, false]'), jsonb_hash('{"b": 1, "aa": [true, {"c": null}]}'), jsonb_hash('[]'), jsonb_hash('{}');
SELECT hash_array(ARRAY[1,2,3]), hash_array(ARRAY[1,NULL,3]), hash_array('{}'::int[]), hash_array(ARRAY[['a','b'],['c','d']]), hash_array(ARRAY[1.5,2.5]);
SELECT hash_array(ARRAY[point(1,2)]);
SELECT hashint4extended(7, 0), hashint4extended(7, 42), hashint8extended(-7, 42), hashtextextended('abc', 42), hashbpcharextended('ab  ', 1), hashfloat8extended(0, 5), hash_numeric_extended(12.50, 9), hash_numeric_extended(0, 9), timetz_hash_extended('01:02:03+02', 3), interval_hash_extended('1 mon', 3), hashinetextended('10.1.2.3/8', 7);
SELECT jsonb_hash_extended('null', 0), jsonb_hash_extended('null', 5), jsonb_hash_extended('true', 5), jsonb_hash_extended('true', 0), jsonb_hash_extended('[1, "a", null, false]', 77), jsonb_hash_extended('{"b": 1, "aa": [true, {"c": null}]}', 77), hash_array_extended(ARRAY[1,2,3], 0), hash_array_extended(ARRAY[1,NULL,3], 11), hashoidvectorextended('1 2 3', 4), hashtidextended('(1,2)', 4), hashmacaddrextended('08:00:2b:01:02:03', 4), uuid_hash_extended('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', 4), hashboolextended(true, 4), pg_lsn_hash_extended('16/B374D848', 4);
SELECT hashtext(NULL), hashint4extended(1, NULL);
