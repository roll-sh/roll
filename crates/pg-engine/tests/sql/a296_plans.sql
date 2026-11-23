-- agent 296: a user aggregate's transition and final functions cost their procost, a state
-- shared by two calls once (get_agg_clause_costs); a Result over a ProjectSet costs the
-- level's target list with the calls in it; equivalence classes, ORDER BY keys and Merge
-- Append members match an index's order by collation identity, and an ordered Append is
-- made only for an order a member already has; text || non-text is anytextcat inlined; a
-- grouping's scan returns the group key expressions at their widths; a domain's column has
-- no typmod (width); a hashed key a subquery's projection computes shows as the expression;
-- IS JSON is boolvarsel's and costs nothing; ^@ is prefixsel; an inheritance tree's Append
-- has a Result over its projection; a subquery scan with unread columns costs a row each.
create table a296n (one int);
insert into a296n values (1), (3), (1);
create function a296_tf(int8, int4) returns int8 as 'select $1 + $2' language sql strict immutable;
create function a296_ff(int8) returns int8 as 'select $1 * 2' language sql strict immutable;
create aggregate a296_sum(int4) (stype = int8, sfunc = a296_tf, initcond = '0');
create aggregate a296_dbl(int4) (stype = int8, sfunc = a296_tf, finalfunc = a296_ff, initcond = '0');
explain select a296_sum(one) from a296n;
explain select a296_sum(one), a296_dbl(one) from a296n;
select a296_sum(one), a296_dbl(one) from a296n;
explain select abs(unnest(array[1, 2, null, -3]));
explain select generate_series(1, 3) as x, generate_series(1, 3) + 1 as xp1;
create table a296c (t text, x int) with (autovacuum_enabled = off);
create table a296d (t text, y int) with (autovacuum_enabled = off);
insert into a296c select 'v' || (i % 500), i from generate_series(1, 1000) i;
insert into a296d select 'v' || (i % 700), i from generate_series(1, 1000) i;
create index a296c_t on a296c (t collate "C");
create index a296d_t on a296d (t collate "C");
analyze a296c;
analyze a296d;
set enable_hashjoin = off;
set enable_nestloop = off;
explain select * from a296c join a296d on a296c.t = a296d.t;
explain select * from a296c join a296d on a296c.t collate "C" = a296d.t collate "C";
explain select * from a296c join a296d on a296c.t = a296d.t order by a296c.t collate "C";
reset enable_hashjoin;
reset enable_nestloop;
create table a296p (t text, v int) partition by range (t);
create table a296p1 partition of a296p for values from ('a') to ('m');
create table a296p2 partition of a296p for values from ('m') to ('zzzz');
insert into a296p select chr(97 + i % 26) || i, i from generate_series(1, 2000) i;
create index on a296p1 (t collate "C");
create index on a296p2 (t collate "C");
analyze a296p;
explain select * from a296p order by t limit 10;
explain (costs off) select * from a296p order by t collate "C" limit 10;
explain select 'a' || g, g || 'b' from generate_series(1, 3) g;
create domain a296dom as char(2);
create table a296m (a a296dom, b char(2), js text) with (autovacuum_enabled = off);
explain select * from a296m;
explain select count(b), b || 'x' from a296m group by b || 'x';
explain select * from a296m where js is json;
explain select r, count(*) from (select random() r from generate_series(1, 10)) ss group by r;
create table a296s (s text) with (autovacuum_enabled = off);
insert into a296s select chr(65 + i % 26) || 'orth' || i from generate_series(1, 2600) i;
analyze a296s;
explain select count(*) from a296s where s ^@ 'Wo';
create table a296i (a int, b text) with (autovacuum_enabled = off);
create table a296i2 () inherits (a296i);
explain select a, length(b) from a296i;
explain select count(*) from (select x from a296c union select length(t) from a296c) ss;
