-- Round 16, agent 25: subqueries selecting some of a table's columns pulled up (their
-- parameterized paths), WHERE clauses over single-table and join subqueries under
-- outer joins, LATERAL join order restrictions, Memoize over subquery outputs,
-- PlaceHolderVar join selectivity, constant equivalence classes in the join search.
create table a25_i4 (f1 int);
create table a25_i8 (q1 int8, q2 int8);
create table a25_tx (f1 text);
create table a25_tk (unique1 int, unique2 int, ten int, stringu2 name, string4 name);
create table a25_u (f1 text unique);
insert into a25_i4 values (0), (123456), (-123456), (2147483647), (-2147483647);
insert into a25_i8 values (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
insert into a25_tx values ('doh!'), ('hi de ho neighbor');
insert into a25_tk select g, (g * 7919) % 10000, g % 10, 'x' || (g % 26), 's' || (g % 4)
  from generate_series(0, 9999) g;
create unique index a25_tk_unique2 on a25_tk (unique2);
analyze a25_i4;
analyze a25_i8;
analyze a25_tx;
analyze a25_tk;
analyze a25_u;

-- A subquery selecting some columns of one table: the table itself in the join tree,
-- with its parameterized index paths.
explain (costs off)
select s.f1 from a25_i4 s join (select unique2 as j from a25_tk i2) ss on ss.j = s.f1;
explain (costs off)
select s.f1, ss.j from a25_i4 s left join (select unique2 as j from a25_tk i2) ss on ss.j = s.f1;
explain (verbose, costs off)
select foo1.join_key as foo1_id, foo3.join_key as foo3_id, bug_field from
  (values (0), (1)) foo1(join_key)
left join
  (select join_key, bug_field from
    (select ss1.join_key, ss1.bug_field from
      (select f1 as join_key, 666 as bug_field from a25_i4 i1) ss1
    ) foo2
   left join
    (select unique2 as join_key from a25_tk i2) ss2
   using (join_key)
  ) foo3
using (join_key);
select foo1.join_key as foo1_id, foo3.join_key as foo3_id, bug_field from
  (values (0), (1)) foo1(join_key)
left join
  (select join_key, bug_field from
    (select ss1.join_key, ss1.bug_field from
      (select f1 as join_key, 666 as bug_field from a25_i4 i1) ss1
    ) foo2
   left join
    (select unique2 as join_key from a25_tk i2) ss2
   using (join_key)
  ) foo3
using (join_key);

-- A Memoize over a LATERAL subquery keyed by another subquery's outputs: their
-- distinct counts are that subquery's rows (get_variable_numdistinct).
explain (verbose, costs off)
select * from
  a25_tx t1
  left join a25_i8 i8
  on i8.q2 = 123,
  lateral (select i8.q1, t2.f1 from a25_tx t2 limit 1) as ss1,
  lateral (select ss1.* from a25_tx t3 limit 1) as ss2
where t1.f1 = ss2.f1;
select * from
  a25_tx t1
  left join a25_i8 i8
  on i8.q2 = 123,
  lateral (select i8.q1, t2.f1 from a25_tx t2 limit 1) as ss1,
  lateral (select ss1.* from a25_tx t3 limit 1) as ss2
where t1.f1 = ss2.f1;

-- A WHERE over a pulled-up subquery an outer join reduced away: the outer join's own
-- clause reads the expression itself, the WHERE the PlaceHolderVar at its relation.
explain (costs off)
select t0.* from a25_tx t0
  left join (select case t1.ten when 0 then 'doh!'::text else null::text end as case1,
                    t1.stringu2
               from a25_tk t1) ss
  on t0.f1 = ss.case1
where ss.stringu2 !~* ss.case1;
explain (costs off)
select t0.* from a25_tx t0
  left join (select case t1.ten when 0 then 'doh!'::text else null::text end as case1,
                    t1.stringu2
               from a25_tk t1
               join a25_i4 i4 on i4.f1 = t1.unique2
               left join a25_u u1 on u1.f1 = t1.string4) ss
  on t0.f1 = ss.case1
where ss.stringu2 !~* ss.case1;
select t0.* from a25_tx t0
  left join (select case t1.ten when 0 then 'doh!'::text else null::text end as case1,
                    t1.stringu2
               from a25_tk t1
               join a25_i4 i4 on i4.f1 = t1.unique2
               left join a25_u u1 on u1.f1 = t1.string4) ss
  on t0.f1 = ss.case1
where ss.stringu2 !~* ss.case1;

-- A constant PlaceHolderVar in a join clause: no statistics, the default distinct
-- count (examine_variable over its evaluation relations).
explain (verbose, costs off)
select t1.* from
  a25_tx t1
  left join (select *, '***'::text as d1 from a25_i8 i8b1) b1
    left join a25_i8 i8
      left join (select *, null::int as d2 from a25_i8 i8b2, a25_i4 i4b2) b2
      on (i8.q1 = b2.q1)
    on (b2.d2 = b1.q2)
  on (t1.f1 = b1.d1)
  left join a25_i4 i4
  on (i8.q2 = i4.f1);

-- Items a class with a constant equates are joinable (have_relevant_eclass_joinclause),
-- a PlaceHolderVar's join computing it in the join relation's target list order.
explain (verbose, costs off)
select ss2.* from
  a25_i4 i41
  left join a25_i8 i8
    join (select i42.f1 as c1, i43.f1 as c2, 42 as c3
          from a25_i4 i42, a25_i4 i43) ss1
    on i8.q1 = ss1.c2
  on i41.f1 = ss1.c1,
  lateral (select i41.*, i8.*, ss1.* from a25_tx limit 1) ss2
where ss1.c2 = 0;
select ss2.* from
  a25_i4 i41
  left join a25_i8 i8
    join (select i42.f1 as c1, i43.f1 as c2, 42 as c3
          from a25_i4 i42, a25_i4 i43) ss1
    on i8.q1 = ss1.c2
  on i41.f1 = ss1.c1,
  lateral (select i41.*, i8.*, ss1.* from a25_tx limit 1) ss2
where ss1.c2 = 0;

drop table a25_i4, a25_i8, a25_tx, a25_tk, a25_u;
