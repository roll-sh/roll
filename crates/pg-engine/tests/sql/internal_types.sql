-- gtsvector (a GiST tsvector index's signature) and the BRIN summary types: in pg_type with
-- their I/O functions, but their input functions reject every value.
select oid, typname, typlen, typbyval, typtype, typcategory, typinput, typoutput, typreceive, typsend, typalign, typstorage, typarray, typelem from pg_type where oid in (3642, 3644, 4600, 4601) order by oid;
select 'x'::gtsvector;
select 'x'::pg_brin_bloom_summary;
select 'x'::pg_brin_minmax_multi_summary;
select cast('x' as gtsvector);
select 'x'::text::gtsvector;
select 'x'::unknown::pg_brin_bloom_summary;
select null::gtsvector, null::pg_brin_bloom_summary is null, null::pg_brin_minmax_multi_summary;
select pg_typeof(null::gtsvector), pg_typeof(null::pg_brin_bloom_summary), pg_typeof(null::pg_brin_minmax_multi_summary);
select null::text::gtsvector, null::pg_brin_bloom_summary::text;
select null::pg_brin_bloom_summary::bytea;
select 'x'::bytea::pg_brin_bloom_summary;
select format_type('gtsvector'::regtype, -1), format_type(4600, null), format_type(4601, -1), format_type(3644, -1);
select 'gtsvector'::regtype::oid, '_gtsvector'::regtype, 'gtsvector[]'::regtype::oid, 'pg_brin_bloom_summary'::regtype::oid;
-- gtsvector has an array type; the summaries don't.
select '{}'::gtsvector[], '{NULL}'::gtsvector[], pg_typeof('{}'::gtsvector[]);
select '{x}'::gtsvector[];
select array[null::gtsvector];
select pg_typeof(array[null::pg_brin_minmax_multi_summary]);
select '{}'::pg_brin_bloom_summary[];
select pg_typeof(array[null::pg_node_tree]);
select '{}'::pg_node_tree[];
-- The I/O functions.
select gtsvectorin('x');
select brin_bloom_summary_in('x');
select brin_minmax_multi_summary_in('x');
select gtsvectorin(null), gtsvectorout(null), brin_bloom_summary_out(null), brin_minmax_multi_summary_out(null);
select brin_bloom_summary_send(null), pg_typeof(brin_minmax_multi_summary_send(null)), pg_typeof(gtsvectorout(null));
select gtsvectorin('x'::text);
select brin_bloom_summary_recv(null);
\fcall gtsvectorout(gtsvector) 'x';
\fcall gtsvectorout(gtsvector) x:00;
\fcall brin_bloom_summary_out(pg_brin_bloom_summary) x:00;
\fcall brin_minmax_multi_summary_out(pg_brin_minmax_multi_summary) x:00;
\fcall gtsvectorout(gtsvector) NULL;
-- Columns of them hold NULLs; they have no btree opclass or ordering.
create table gxc_t (a gtsvector, b pg_brin_bloom_summary, c pg_brin_minmax_multi_summary);
insert into gxc_t values (null, null, null);
select * from gxc_t;
select a::text, b::text, c::text from gxc_t;
create index on gxc_t (a);
select a from gxc_t order by a;
select b from gxc_t order by 1;
select null::gtsvector = null::gtsvector;
select row(null::gtsvector), null::gtsvector || 'a';
select length(null::gtsvector);
select format('%s', null::gtsvector), xmlelement(name a, null::gtsvector), to_json(null::gtsvector), row_to_json(row(null::gtsvector));
create domain gxc_d as gtsvector;
select null::gxc_d;
drop domain gxc_d;
drop table gxc_t;
-- Xml has no ordering operator either.
select x from (values ('<a/>'::xml)) t(x) order by x;
-- Grouping, DISTINCT, set operations and PARTITION BY need a type's equality (from a
-- default btree or hash opclass): gtsvector, json, xml, jsonpath, the geometric types but
-- not int2vector (it borrows the array opclass), and named composites with such fields.
create table gxc_t (a gtsvector, b pg_brin_bloom_summary);
select a from gxc_t group by a;
select count(distinct b) from gxc_t;
select distinct a from gxc_t;
select x from (values (null::gtsvector)) t(x) group by grouping sets ((x), ());
drop table gxc_t;
select distinct '$'::jsonpath;
select distinct null::refcursor;
select distinct '{}'::json[];
select distinct row('{}'::json);
select distinct x from (values (array['{}'::json])) t(x);
select 1 from (values ('{}'::json)) t(x) group by x;
select 1 from (values ('{}'::json)) t(x) group by 1;
select x from (values ('{}'::json)) t(x) group by 1;
select count(distinct x) from (values ('{}'::json)) t(x);
select x from (values ('{}'::json)) t(x) union select '{}'::json;
select x from (values ('{}'::json)) t(x) intersect all select '{}'::json;
select x from (values ('{}'::json)) t(x) union all select '{}'::json;
select 1, x from (values ('{}'::json)) t(x) except select 1, '{}'::json;
(select 1, x from (values ('{}'::json)) t(x) union all select 2, '{}'::json) union select 1, '{}';
values ('{}'::json) union values ('{}'::json);
select distinct on (x) 1 from (values ('{}'::json)) t(x);
select x, row_number() over (partition by x) from (values ('{}'::json)) t(x);
select x, row_number() over w from (values ('{}'::json)) t(x) window w as (partition by x);
select array_agg(distinct x) from (values ('{}'::json)) t(x);
select distinct null::pg_snapshot;
select distinct null::box;
select distinct null::unknown;
select distinct null::cstring;
select distinct * from (values (1, '{}'::json)) t(a, b);
select distinct a, * from (values (1, '{}'::json)) t(a, b);
select distinct 1, point(1,1);
select distinct indkey from pg_index limit 0;
select string_agg(distinct x::text, ',') from (values ('{}'::json)) t(x);
select count(distinct (x, 1)) from (values ('{}'::json)) t(x);
create table gxc_j (j json, x xml);
select distinct j from gxc_j;
select distinct x from gxc_j;
select j, count(*) from gxc_j group by j;
select distinct gxc_j from gxc_j;
drop table gxc_j;
