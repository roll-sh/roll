-- Round 16, agent 30: JOIN USING merged columns as PlaceHolderVars at their join
-- (add_nullingrels_if_needed), classes with a constant through a reduced outer join and a
-- nested inner join, PlaceHolderVar nested loop parameters computed by the outer side
-- (create_nestloop_plan), join clauses relevant by required_relids overlap.
create table a30_i4 (f1 int);
create table a30_i8 (q1 int8, q2 int8);
create table a30_tk (unique1 int, ten int, fivethous int, tenthous int);
insert into a30_i4 values (0), (123), (-123456), (64), (7);
create table a30_i0 (f1 int);
insert into a30_i0 values (0), (64), (999), (3), (null);
insert into a30_i8 values (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (64, 123), (4567890123456789, -4567890123456789);
insert into a30_tk select g, g % 10, g % 5000, g from generate_series(0, 9999) g;
create index a30_tk_tenthous on a30_tk (tenthous);
analyze a30_i4;
analyze a30_i0;
analyze a30_i8;
analyze a30_tk;

-- A variable-free USING alias under a left join: one PlaceHolderVar `'123'::bigint`
-- computed at the join of the two subqueries, not the subquery's `123` at its scan.
explain (verbose, costs off)
select * from
a30_i4 i0 left join
( (select *, 123 as x from a30_i4 i1) ss1
  left join
  (select *, q2 as x from a30_i8 i2) ss2
  using (x)
) ss0
on (i0.f1 = ss0.f1)
order by i0.f1, x;
select * from
a30_i0 i0 left join
( (select *, 123 as x from a30_i4 i1) ss1
  left join
  (select *, q2 as x from a30_i8 i2) ss2
  using (x)
) ss0
on (i0.f1 = ss0.f1)
order by i0.f1, x;
-- The same under FULL and RIGHT joins, NULL-extended rows on every side.
select * from
a30_i0 i0 left join
( (select *, f1 + 1 as x from a30_i4 i1) ss1
  full join
  (select *, q2 as x from a30_i8 i2) ss2
  using (x)
) ss0
on (i0.f1 = ss0.f1)
order by 1, 2, 3, 4, 5;
select * from
a30_i0 i0 left join
( (select *, 7 as x from a30_i4 i1) ss1
  right join
  (select *, 7::int8 as x from a30_i8 i2) ss2
  using (x)
) ss0
on (i0.f1 = ss0.f1 + 4)
order by 1, 2, 3, 4, 5;
select * from
a30_i8 i0 right join
( (select *, 123 as x from a30_i4 i1) ss1
  left join
  (select *, q1 as x from a30_i8 i2) ss2
  using (x)
) ss0
on (i0.q2 = ss0.x)
order by 1, 2, 3, 4, 5;

-- A constant a pulled-up subquery equates a nested inner join's column with: the class
-- reaches through the outer join a WHERE made inner (`i8.q1` gets it too).
select * from a30_i8 i8 left join (a30_i4 i4 join
  (select 64::information_schema.cardinal_number as d1 from a30_tk t3 where t3.fivethous < 2) ss
  on i4.f1 = ss.d1) on i8.q1 = i4.f1 where i4.f1 > 0;
select * from a30_i8 i8 left join (a30_i4 i4 join
  (select 64::information_schema.cardinal_number as d1 from a30_tk t3 where t3.fivethous < 2) ss
  on i4.f1 = ss.d1) on i8.q1 = i4.f1
order by 1, 2, 3, 4;

-- A PlaceHolderVar over a LATERAL subquery's relations as a parameter of an inner index
-- scan (join 345's shape): rows, NULL-extended ones too.
select ss1.d1, i8.q2 from
  a30_tk as t1
  inner join a30_tk as t2 on t1.tenthous = t2.tenthous
  inner join
    a30_i8 as i8
    left join a30_i4 as i4
      inner join (select 64::information_schema.cardinal_number as d1
                  from a30_tk t3,
                       lateral (select abs(t3.unique1) + random()) ss0(x)
                  where t3.fivethous < 2) as ss1
      on i4.f1 = ss1.d1
    on i8.q1 = i4.f1
  on t1.tenthous = ss1.d1
where t1.unique1 <= i4.f1
order by 1, 2;
select ss1.d1, i8.q2, i4.f1 from
  a30_i8 as i8
  left join a30_i4 as i4
    inner join (select 64::information_schema.cardinal_number as d1
                from a30_tk t3,
                     lateral (select abs(t3.unique1) + random()) ss0(x)
                where t3.fivethous < 2) as ss1
    on i4.f1 = ss1.d1
  on i8.q1 = i4.f1
order by 1, 2, 3;

drop table a30_i0, a30_i4, a30_i8, a30_tk;
