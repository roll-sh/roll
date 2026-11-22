-- agent 288: a constant reached through a chain of left joins gives the next one's nullable
-- side its key's constant too, a view's table included (reconsider_outer_join_clauses until
-- nothing changes); a CTE's or subquery's columns keep their statistics but are never unique
-- (examine_simple_variable); one grouping set is a plain GROUP BY (parseCheckAggregates);
-- grouping sets all hashed are a HashAggregate; `(a, b) < ANY (SELECT ...)` is a semi join
-- on a row comparison; a FROM call of a SQL function folding to a constant WITH ORDINALITY
-- or in ROWS FROM is the constant (an expression's Function Scan shows only its alias);
-- text to cidr is an I/O coercion; a sort key shows the collation its expression derives;
-- a table's whole-row Var is a tuple header and its columns wide; `x = ANY (constant
-- array)` is folded; json_populate_record returns no set; a domain's own collation shows
-- no COLLATE; TABLESAMPLE scans are sized by their sampled pages and tuples.
create table a288z1 (f1 int primary key);
create table a288z2 (f2 int primary key);
create table a288z3 (f3 int primary key);
insert into a288z1 values (53);
insert into a288z2 values (53);
create view a288zv as select *, 'dummy'::text as junk from a288z1;
explain select * from a288z2 left join a288z3 on (f2 = f3) left join a288zv on (f3 = f1) where f2 = 53;
explain select * from a288z2 left join a288z3 on (f2 = f3) left join a288z1 on (f3 = f1) where f2 = 53;
select * from a288z2 left join a288z3 on (f2 = f3) left join a288zv on (f3 = f1) where f2 = 53;
create table a288u (inb bytea, d text primary key) with (autovacuum_enabled = off);
insert into a288u select null, g::text from generate_series(1, 880) g;
explain with b as materialized (select d, inb from a288u) select * from b b1 join b b2 using (d);
explain select * from (select d from a288u offset 0) p join (select d from a288u offset 0) q using (d);
create table a288gs (id int, v int, x xid) with (autovacuum_enabled = off);
explain select x, count(*) from a288gs group by grouping sets ((x), (x)) order by x::text;
explain select a, b from (values (1, 2), (2, 3)) v(a, b) group by a, b, grouping sets (a);
select a, grouping(a), sum(b) from (values (1, 2), (1, 3)) v(a, b) group by a, grouping sets (a);
create table a288t (u1 int, u2 int, h int, ten int) with (autovacuum_enabled = off);
insert into a288t select g, 1000 - g, g % 100, g % 10 from generate_series(0, 999) g;
create index a288t_u1 on a288t (u1);
analyze a288t;
explain select u1, u2 from a288t where (u1, u2) < any (select ten, ten from a288t where h < 3) and u1 <= 20 order by 1;
select u1, u2 from a288t where (u1, u2) < any (select ten, ten from a288t where h < 3) and u1 <= 20 order by 1;
select (1, 2) < any (select 1, 3), (1, 2) <> any (select 1, 2), (2, 1) < any (select 1, 3);
create function a288f1(int) returns int as 'select $1' language sql;
explain select * from a288f1(1) with ordinality as t1(v, o);
select * from a288f1(1) with ordinality as t1(v, o);
explain select * from rows from (a288f1(2), generate_series(1, 2)) as r;
explain with a(b) as (values (row(1, 2, 3))) select * from a, coalesce(b) as c(i int, j int, k int);
create table a288i (c inet) with (autovacuum_enabled = off);
explain select set_masklen(cidr(text(c)), 24) from a288i;
create table a288c (a int, b text collate "C", y text collate "POSIX") with (autovacuum_enabled = off);
explain select a, lower(y) from a288c order by lower(y), a;
explain select a, coalesce(b, 'foo') from a288c order by 2;
explain select array_agg(b order by b) from a288c;
explain select a, b from a288c union all select a, b from a288c order by 2;
explain select a288c from a288c;
explain select 33 = any (null::int[]), 33 = any ('{1,2}'::int[]);
create type a288r as (a int, b text);
create table a288j (js json) with (autovacuum_enabled = off);
explain select json_populate_record(null::a288r, '{"a": 0, "b": 1}');
explain select (json_populate_record(null::a288r, js)).* from a288j;
select json_populate_record(null::a288r, '{"a": 0, "b": "x"}');
create domain a288dp as text collate "POSIX";
explain select a, b::a288dp from a288c order by 2;
explain select a, y::a288dp from a288c order by 2;
create table a288s (id int) with (fillfactor = 10, autovacuum_enabled = off);
insert into a288s select g from generate_series(0, 119) g;
analyze a288s;
explain select id from a288s tablesample system (50) repeatable (0);
explain select id from a288s tablesample bernoulli (25) where id > 10;
explain select * from (values (0), (100)) v(pct), lateral (select count(*) from a288s tablesample bernoulli (pct)) ss;
