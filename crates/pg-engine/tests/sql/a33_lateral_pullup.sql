-- Round 16, agent 33: LATERAL subqueries over joins pulled up (pull_up_simple_subquery),
-- their outputs PlaceHolderVars with ph_lateral, the relations inside parameterized by the
-- LATERAL references (create_lateral_join_info), and the rows they give: under LEFT and
-- FULL joins, null-extended rows, volatile select lists (not pulled up), aggregates over
-- the subquery, nested LATERAL items.
create table a33_a (id int, v int, t text);
create table a33_b (id int, w int);
create table a33_c (id int, x int);
insert into a33_a values (1, 10, 'a'), (2, 20, 'b'), (3, null, 'c'), (4, 40, null), (null, 50, 'e');
insert into a33_b values (1, 100), (2, 200), (2, 201), (5, 500), (null, 600);
insert into a33_c values (1, 1000), (2, 2000), (3, 3000), (5, 5000), (null, 7000);
create index a33_b_id on a33_b (id);
create index a33_c_id on a33_c (id);
analyze a33_a;
analyze a33_b;
analyze a33_c;

-- A LEFT JOIN LATERAL subquery over an inner join: its PlaceHolderVar `least(...)` goes
-- NULL for the outer rows without a match.
explain (costs off)
select a.id, ss.* from a33_a a left join lateral
  (select b.id as bid, c.x, least(a.v, b.w, c.x) as l from a33_b b join a33_c c on b.id = c.id) ss
  on a.id = ss.bid order by 1, 2, 3, 4;
select a.id, ss.* from a33_a a left join lateral
  (select b.id as bid, c.x, least(a.v, b.w, c.x) as l from a33_b b join a33_c c on b.id = c.id) ss
  on a.id = ss.bid order by 1, 2, 3, 4;

-- A constant and a bare LATERAL reference among the outputs: PlaceHolderVars too.
select a.id, ss.* from a33_a a left join lateral
  (select 1 as one, a.v as av, coalesce(a.t, 'none') as at, b.w from a33_b b join a33_c c on b.id = c.id) ss
  on a.id = ss.w / 100 order by 1, 2, 3, 4, 5;

-- The subquery's WHERE reading the LATERAL value (a join clause of the group).
explain (costs off)
select a.id, ss.* from a33_a a left join lateral
  (select b.w, c.x from a33_b b join a33_c c on b.id = c.id where c.x > a.v * 50) ss
  on true order by 1, 2, 3;
select a.id, ss.* from a33_a a left join lateral
  (select b.w, c.x from a33_b b join a33_c c on b.id = c.id where c.x > a.v * 50) ss
  on true order by 1, 2, 3;

-- Inner LATERAL (a comma join), its WHERE comparing a LATERAL value with an inner column.
select a.id, ss.* from a33_a a,
  lateral (select a.v as lat, b.w as loc from a33_b b, a33_c c where b.id = c.id) ss
  where ss.loc = ss.lat * 10 order by 1, 2, 3;

-- Under a FULL JOIN above the LATERAL join.
select * from
  (select a.id, ss.l from a33_a a left join lateral
     (select least(a.v, b.w) as l, b.id from a33_b b join a33_c c on b.id = c.id) ss
     on a.id = ss.id) q
  full join a33_c c2 on c2.id = q.id
  order by 1, 2, 3, 4;

-- The LATERAL subquery under a RIGHT JOIN of its own with a PlaceHolderVar needed above.
select a.id, ss.* from a33_a a,
  lateral (select 1 as x, s1.lat from
     (select a.v as lat, b.w as loc from a33_b b, a33_c c where b.id = c.id) s1
     right join a33_c c2 on (c2.x > 2500)
     where s1.loc = s1.lat * 10) ss
  where a.id > 0 order by 1, 2, 3;

-- A volatile select list keeps the subquery a LATERAL item (is_simple_subquery).
select count(*) from a33_a a left join lateral
  (select b.w, c.x, random() >= 0 as r from a33_b b join a33_c c on b.id = c.id) ss
  on a.id = ss.w / 100 where ss.r is not false;

-- Aggregates over the pulled-up subquery's outputs, PlaceHolderVars among them.
select a.id, count(ss.l), sum(ss.l), max(ss.k) from a33_a a left join lateral
  (select least(a.v, b.w) as l, coalesce(a.v, -1) as k from a33_b b join a33_c c on b.id = c.id) ss
  on a.id = ss.l / 10 group by a.id order by 1;

-- Nested LATERAL: a LATERAL subquery reading an outer LATERAL subquery's output.
select a.id, s1.w, s2.* from a33_a a
  left join lateral (select b.w, b.id from a33_b b join a33_c c on b.id = c.id where b.id = a.id) s1 on true
  left join lateral (select s1.w + c.x as sx, a.v as av from a33_c c join a33_b b2 on c.id = b2.id where c.id = s1.id) s2 on true
  order by 1, 2, 3, 4;

-- A LATERAL subquery whose join has an outer join of its own and a lateral clause in its
-- ON (bug #17768's shape): not pulled up through the outer join.
select a.id, ss.* from a33_a a,
  lateral (select b.w, c.x from a33_b b inner join a33_c c on a.id = 1
           inner join (a33_c c4 left join a33_b b5 on true) on c4.id = c.id and b5.id = b.id) ss
  order by 1, 2, 3;

-- SELECT * of the join (no select list of its own).
explain (costs off)
select * from a33_a a, lateral (select * from a33_b b inner join a33_c c on a.id = 1) ss;
select * from a33_a a, lateral (select * from a33_b b inner join a33_c c on a.id = 1) ss
  order by 1, 2, 3, 4, 5, 6, 7;

-- A function scan LATERAL with a LATERAL value output (its column first, then the value).
explain (verbose, costs off)
select * from a33_a a left join lateral (select a.id as aid, * from generate_series(1, 2) g) s on true;
select * from a33_a a left join lateral (select a.id as aid, * from generate_series(1, 2) g) s on true
  order by 1, 2, 3, 4, 5;

-- The subquery's outputs nobody reads: its LATERAL value gone, no PlaceHolderVar.
select a.id from a33_a a left join lateral
  (select b.w, least(a.v, b.w) as l from a33_b b join a33_c c on b.id = c.id) ss
  on a.id = ss.w / 100 where (a.v + coalesce(ss.w, 0)) > 0 order by 1;

drop table a33_a, a33_b, a33_c;
