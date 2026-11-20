-- rangesel / multirangesel from the bounds and length histograms; multirangesel's `<@` of a
-- range without the length histogram; an array of text's elements padded to their alignment
-- (construct_md_array); a stable call of constants evaluated for an estimate
-- (estimate_expression_value).
create table a273_r (r int4range, m int4multirange, d daterange) with (autovacuum_enabled = off);
insert into a273_r select int4range(g, g + (g % 37)), int4multirange(int4range(g, g + 5), int4range(g + 10, g + (g % 50) + 11)), daterange('2020-01-01'::date + g, '2020-01-01'::date + g + (g % 9)) from generate_series(1, 3000) g;
insert into a273_r select 'empty', '{}', 'empty' from generate_series(1, 100);
insert into a273_r select null, null, null from generate_series(1, 50);
analyze a273_r;
explain select * from a273_r where r && int4range(100, 200);
explain select * from a273_r where r @> int4range(100, 105);
explain select * from a273_r where r <@ int4range(100, 400);
explain select * from a273_r where 150 <@ r;
explain select * from a273_r where r >> int4range(500, 600);
explain select * from a273_r where int4range(500, 600) &< r;
explain select * from a273_r where r < int4range(500, 600);
explain select * from a273_r where r <@ 'empty'::int4range;
explain select * from a273_r where m && '{[100,200), [400,500)}'::int4multirange;
explain select * from a273_r where m @> int4range(100, 102);
explain select * from a273_r where m <@ int4range(100, 900);
explain select * from a273_r where m <@ '{[100,900)}'::int4multirange;
explain select * from a273_r where d @> '2020-03-05'::date;
create table a273_a (t text[], v varchar[]) with (autovacuum_enabled = off);
insert into a273_a select array['x' || g, 'yy'], array['z' || (g % 7)] from generate_series(1, 500) g;
analyze a273_a;
select pg_column_size(t), pg_column_size(v) from a273_a limit 2;
select attname, avg_width from pg_stats where tablename = 'a273_a' order by 1;
explain select * from a273_a;
create table a273_ts (a tsvector) with (autovacuum_enabled = off);
insert into a273_ts select to_tsvector('simple', 'w' || (g % 20) || ' z' || (g % 5)) from generate_series(1, 1000) g;
analyze a273_ts;
explain select * from a273_ts where a @@ to_tsquery('simple', 'w3 | z1');
drop table a273_r, a273_a, a273_ts;
