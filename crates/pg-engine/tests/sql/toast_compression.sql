-- TOAST: compression of every varlena type (bytea, jsonb, arrays, xml, text), pglz and
-- lz4 (column COMPRESSION, default_toast_compression), and how values are stored as
-- pg_column_size / pg_column_compression report them.
create table tz (id int, t text, b bytea, j jsonb, a int[], x xml, n numeric);
insert into tz select g, repeat('abc', 3000), convert_to(repeat('xyz', 3000), 'UTF8'), to_jsonb(repeat('q', 5000)), array_fill(g, array[3000]), xmlparse(content repeat('<a>1</a>', 1000)), 1 from generate_series(1, 20) g;
select pg_relation_size('tz'), pg_table_size('tz'), pg_total_relation_size('tz');
select pg_column_size(t), pg_column_size(b), pg_column_size(j), pg_column_size(a), pg_column_size(x), pg_column_size(id), pg_column_size(n), pg_column_compression(t), pg_column_compression(a), pg_column_compression(id) from tz where id = 1;
create table tj (j jsonb, a text[], f float8[], i int8[]);
insert into tj select jsonb_build_object('k', g, 'arr', (select jsonb_agg(x) from generate_series(1, 400) x), 'n', 1.5, 'b', true, 'z', null, 's', repeat('w', 100)), array_fill('hello'::text, array[600]), array_fill(1.5::float8, array[600]), array_fill(g::int8, array[20, 30]) from generate_series(1, 20) g;
select pg_table_size('tj');
insert into tj select (select jsonb_object_agg('key' || x, x * 1.25) from generate_series(1, 300) x), array[null, 'a', repeat('b', 3000)], array[1, null, 3]::float8[], null from generate_series(1, 5) g;
select pg_table_size('tj');
select pg_column_size(j), pg_column_size(a), pg_column_compression(j), pg_column_compression(a) from tj order by pg_column_size(j), pg_column_size(a) limit 3;
create table tm (id int, t text, u text);
insert into tm values (1, repeat(md5('a'), 300), 'short'), (2, (select string_agg(md5(x::text), '') from generate_series(1, 300) x), 'x');
select id, pg_column_size(t), pg_column_size(u), pg_column_compression(t), pg_column_compression(u) from tm order by id;
show default_toast_compression;
set default_toast_compression = lz4;
create table tl (id int, t text);
insert into tl select g, repeat('abc', 3000) from generate_series(1, 20) g;
select pg_table_size('tl');
create table tl2 (id int, t text compression pglz, u text compression lz4, v text);
insert into tl2 select g, repeat(md5(g::text), 100), repeat(md5(g::text), 100), repeat(md5(g::text), 100) from generate_series(1, 20) g;
select pg_table_size('tl2');
select pg_column_size(t), pg_column_size(u), pg_column_size(v), pg_column_compression(t), pg_column_compression(u), pg_column_compression(v) from tl2 where id < 4 order by id;
select attname, attcompression from pg_attribute where attrelid = 'tl2'::regclass and attnum > 0 order by attnum;
create table tl3 (id int, t text);
insert into tl3 select g, (select string_agg(md5((g * 1000 + x)::text), '') from generate_series(1, 200) x) from generate_series(1, 30) g;
insert into tl3 select g, (select string_agg(x::text || 'abc' || (x % 7)::text, ',') from generate_series(1, 2000) x) from generate_series(1, 30) g;
select pg_table_size('tl3');
set default_toast_compression = zstd;
reset default_toast_compression;
select pg_column_compression('abc'::text), pg_column_size('abc'::text), pg_column_size(1), pg_column_size(1::int8), pg_column_size('abc'), pg_column_size(repeat('x', 200)), pg_column_size(array[1,2,3]), pg_column_size('{"a":1}'::jsonb), pg_column_size(1.5::numeric), pg_column_size(true), pg_column_size('\x0102'::bytea), pg_column_size(row(1,2)), pg_column_size(null::text);
