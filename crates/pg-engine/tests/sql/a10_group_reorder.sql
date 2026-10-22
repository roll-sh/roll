-- Round 15 (agent 10): GROUP BY keys reordered to the input's order (get_useful_group_keys_
-- orderings) with the ORDER BY's sort counted, a merge join's outer side sorted
-- incrementally (its presorted keys' groups estimated), an index-only scan under an
-- aggregation returning the index's columns, a projection over a UNION ALL merged in
-- order computed by its members (a Result above), Merge Appends of inheritance trees as
-- merge join inputs under a LIMIT.
CREATE TABLE a10_btg AS SELECT
  i % 10 AS x,
  i % 10 AS y,
  'abc' || i % 10 AS z,
  i AS w
FROM generate_series(1, 100) AS i;
CREATE INDEX a10_btg_x_y_idx ON a10_btg(x, y);
ANALYZE a10_btg;
SET enable_hashagg = off;
SET enable_seqscan = off;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a10_btg GROUP BY y, x;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a10_btg GROUP BY z, y, w, x;
SET enable_hashjoin = off;
SET enable_nestloop = off;
EXPLAIN (COSTS OFF)
SELECT count(*)
  FROM a10_btg t1 JOIN a10_btg t2 ON t1.w = t2.w AND t1.x = t2.x AND t1.z = t2.z
  GROUP BY t1.w, t1.z, t1.x;
SELECT t1.x, t1.w, count(*)
  FROM a10_btg t1 JOIN a10_btg t2 ON t1.w = t2.w AND t1.x = t2.x AND t1.z = t2.z
  GROUP BY t1.w, t1.z, t1.x ORDER BY 2 LIMIT 5;
EXPLAIN (COSTS OFF)
SELECT count(*)
  FROM a10_btg t1 JOIN a10_btg t2 ON t1.w = t2.w AND t1.x = t2.x AND t1.z = t2.z;
RESET enable_nestloop;
RESET enable_hashjoin;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a10_btg GROUP BY w, x, z, y ORDER BY y, x, z, w;
SELECT y, x, z, w, count(*) FROM a10_btg GROUP BY w, x, z, y ORDER BY y, x, z, w LIMIT 4;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM a10_btg GROUP BY w, x, y, z ORDER BY x*x, z;
CREATE INDEX a10_btg_y_x_w_idx ON a10_btg(y, x, w);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT y, x, array_agg(distinct w)
  FROM a10_btg WHERE y < 0 GROUP BY x, y;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT x, y, count(*) FROM a10_btg WHERE y < 0 GROUP BY x, y;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT w, x FROM a10_btg WHERE y < 0;
SELECT y, x, array_agg(distinct w ORDER BY w) FROM a10_btg WHERE y < 3 GROUP BY x, y;
RESET enable_seqscan;
RESET enable_hashagg;

create table a10_mt0 (id serial primary key, name varchar);
create table a10_mt3 (id integer primary key) inherits (a10_mt0);
create index a10_mt3i on a10_mt3 ((1-id));
insert into a10_mt3 (name) values ('Test 5'), ('Test 6');
explain (verbose, costs off)
select 1 - id as c from
(select id from a10_mt3 t1 union all select id * 2 from a10_mt3 t2) ss
order by c;
explain (costs off)
select 1 - id as c from
(select id from a10_mt3 t1 union all select id * 2 from a10_mt3 t2) ss
order by c;
explain (verbose, costs off)
select 1 - id as c, id from
(select id from a10_mt3 t1 union all select id * 2 from a10_mt3 t2) ss
order by c;
explain (verbose, costs off)
select id, 1 - id as c from
(select id from a10_mt3 t1 union all select id * 2 from a10_mt3 t2) ss
order by c;
explain (verbose, costs off)
select 1 - id as c from
(select id from a10_mt3 t1 union all select id * 2 from a10_mt3 t2 union all select id + 1 from a10_mt3 t3) ss
order by c limit 1;
select 1 - id as c, id from
(select id from a10_mt3 t1 union all select id * 2 from a10_mt3 t2) ss
order by c;
drop table a10_mt0 cascade;

create table a10_mm0 (a int, b int, c int, d int);
create table a10_mm1 () inherits (a10_mm0);
create index a10_mm0i on a10_mm0 (b, c);
create index a10_mm1i on a10_mm1 (b, c);
insert into a10_mm0 select i, i % 7, i % 3, i % 3 from generate_series(1, 20) i;
insert into a10_mm1 select i, i % 5, i % 2, i % 4 from generate_series(1, 20) i;
set enable_nestloop = off;
explain (costs off)
select t1.* from a10_mm0 t1, a10_mm0 t2
where t1.b = t2.b and t2.c = t2.d
order by t1.b limit 10;
select t1.b, count(*) from a10_mm0 t1, a10_mm0 t2
where t1.b = t2.b and t2.c = t2.d
group by t1.b order by t1.b;
reset enable_nestloop;
drop table a10_mm0 cascade;
drop table a10_btg;
