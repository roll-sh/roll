-- agent 285: ORDER BY matches an index by collation identity (pathkeys' EC collation): in a
-- C database an index on t COLLATE "C" orders no ORDER BY t, and orders ORDER BY t COLLATE
-- "C" (an incremental sort over it too); IN over a FROM subquery planned as a level of its
-- own (DISTINCT ON, GROUP BY, UNION) is a semi join with that subquery, an inner join when it
-- is distinct on the keys (query_is_distinct_for); an IN whose select list reads the outer
-- row is pulled up (a LATERAL subquery); a one-row VALUES with a sub-select is pulled up,
-- the sub-select copied into each place reading it; a SQL function in FROM WITH ORDINALITY
-- is estimated by its ROWS and COST; a set operation's leaf whose columns need a coercion
-- keeps its Subquery Scan; an aggregation over a one-row VALUES reads none of its columns; a
-- VARIADIC array of constants is a constant (the call evaluated); a FROM-less WHERE costs its
-- qual once.
create table a285co (t text, u text collate "C", i int) with (autovacuum_enabled = off);
insert into a285co select g::text, g::text, g from generate_series(1, 2000) g;
create index a285co_t on a285co (t collate "C");
create index a285co_u on a285co (u);
analyze a285co;
explain select * from a285co order by t limit 5;
explain select * from a285co order by t collate "C" limit 5;
explain select * from a285co order by u collate "default" limit 5;
explain select * from a285co order by t collate "C", i limit 5;
explain select * from a285co order by u, t limit 5;
select t from a285co order by t collate "C" limit 3;
create table a285f (id int) with (autovacuum_enabled = off);
create table a285b (id1 int, id2 int) with (autovacuum_enabled = off);
insert into a285f values (1), (2);
insert into a285b values (1, 1), (2, 2), (3, 1);
explain select * from a285f where id in (select id2 from (select id2 from a285b group by id2) s);
select * from a285f where id in (select id2 from (select distinct on (id2) id1, id2 from a285b) s) order by 1;
explain select * from a285f where id in (select id2 from (select distinct id1, id2 from a285b) s);
explain select * from a285f where id in (select id2 from (select id1, id2 from a285b union select id1, id2 from a285b) s);
explain select * from a285f where id in (select id2 from (select id2 from a285b union select id2 from a285b) s);
select * from a285f where id in (select id2 from (select id2 from a285b union select id2 from a285b) s) order by 1;
select id from a285f f where id in (select f.id + id1 - 1 from a285b where id2 = f.id) order by 1;
explain select * from a285co, (values ((select i from (values (3), (2), ((select 7))) v(i) order by i limit 1))) w(i) where a285co.i = w.i;
select * from a285co, (values ((select i from (values (3), (2), ((select 7))) v(i) order by i limit 1))) w(i) where a285co.i = w.i;
create function a285srf(int) returns setof a285b as 'select * from a285b where id1 = $1' language sql;
explain select * from a285srf(1) with ordinality as z(a, b, o);
explain select * from a285f, a285srf(a285f.id) with ordinality as z(a, b, o) where a285f.id = z.b;
explain select 1.1 as two union select 2 order by 1;
explain select id from a285f union all select id1::int8 from a285b;
explain select any_value(v) from (values (null)) as v (v);
select sum(x), count(*) filter (where x > 1), array_agg(x order by x) from (values (5)) t(x);
create function a285least(variadic anyarray) returns anyelement as 'select min($1[i]) from generate_subscripts($1, 1) g(i)' language sql immutable strict;
explain select a285least(10, 1, 20, 33);
select a285least(10, 1, 20, 33);
explain select 1 as one where 1 in (select 1);
drop function a285least(variadic anyarray);
drop function a285srf(int);
drop table a285co, a285f, a285b;
