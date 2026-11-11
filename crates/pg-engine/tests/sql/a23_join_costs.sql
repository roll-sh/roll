-- Round 16 (agent 23): join costing and unique-ification (join 499, 543, 819, 888, subselect
-- 326): pseudoconstant quals, PlaceHolderVar equalities, foreign keys with constants, the
-- unique-ified side of a semi join, join relations' target list order.
create table a23_t (unique1 int, hundred int, thousand int, tenthous int, odd int);
insert into a23_t select i, i % 100, i % 1000, i, (i % 100) * 2 + 1 from generate_series(0, 9999) i;
create index a23_t_hundred on a23_t (hundred);
create index a23_t_thous_tenthous on a23_t (thousand, tenthous);
create index a23_t_unique1 on a23_t (unique1);
vacuum analyze a23_t;
create table a23_o (unique1 int, hundred int);
insert into a23_o select i, i % 100 from generate_series(0, 999) i;
create index a23_o_unique1 on a23_o (unique1);
vacuum analyze a23_o;
create table a23_q (q1 int8, q2 int8);
insert into a23_q values (123, 456), (123, 4567890123456789), (4567890123456789, 123),
  (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
create table a23_f (f1 int4);
insert into a23_f values (0), (123456), (-123456), (2147483647), (-2147483647);

-- A pseudoconstant qual (an outer query's value) gates the join, filtering no rows.
explain (costs off)
select exists(
  select * from a23_q t1 left join
    (a23_q t2 inner join a23_q t3 on x0.f1 = 1
     left join a23_q t4 on t2.q2 = t4.q2)
  on t1.q1 = t2.q1
) from a23_f x0;
select exists(
  select * from a23_q t1 left join
    (a23_q t2 inner join a23_q t3 on x0.f1 = 1
     left join a23_q t4 on t2.q2 = t4.q2)
  on t1.q1 = t2.q1
) from a23_f x0;

-- An equality with a PlaceHolderVar (no statistics) is eqjoinsel's, its distinct count the
-- default.
create table a23_inner (id int8 primary key, dat1 int8);
insert into a23_inner values (123, 42);
explain (costs off)
select f1, x from a23_f
  join ((select 42 as x from a23_q left join a23_inner on q1 = id) as ss1
        right join a23_t on null)
  on a23_t.unique1 = ss1.x or a23_t.hundred = ss1.x;
select f1, x from a23_f
  join ((select 42 as x from a23_q left join a23_inner on q1 = id) as ss1
        right join a23_t on null)
  on a23_t.unique1 = ss1.x or a23_t.hundred = ss1.x;

-- A foreign key one of whose columns is in a class with a constant: the referencing side's
-- restriction is divided out of its selectivity.
create table a23_fk (x integer, x10 integer, x10b integer, x100 integer);
insert into a23_fk select x, x/10, x/10, x/100 from generate_series(1, 1000) x;
create unique index on a23_fk (x, x10, x100);
analyze a23_fk;
alter table a23_fk add constraint a23_fk_fk foreign key (x, x10b, x100) references a23_fk (x, x10, x100);
explain (costs off)
select * from a23_fk f1
  join a23_fk f2 on (f1.x = f2.x and f1.x10 = f2.x10b and f1.x100 = f2.x100)
  join a23_fk f3 on f1.x = f3.x
  where f1.x100 = 2;
select count(*) from a23_fk f1
  join a23_fk f2 on (f1.x = f2.x and f1.x10 = f2.x10b and f1.x100 = f2.x100)
  join a23_fk f3 on f1.x = f3.x
  where f1.x100 = 2;

-- The semi join's unique-ified side is unique for a join only with all of its left side;
-- it is sorted or hashed whole (create_unique_path), and the semi join's clauses
-- parameterize scans of its left side.
explain (verbose, costs off)
select t1.unique1, t2.hundred
from a23_o t1, a23_t t2
where exists (select 1 from a23_t t3
              where t3.thousand = t1.unique1 and t3.tenthous = t2.hundred)
      and t1.unique1 < 1;
select t1.unique1, t2.hundred
from a23_o t1, a23_t t2
where exists (select 1 from a23_t t3
              where t3.thousand = t1.unique1 and t3.tenthous = t2.hundred)
      and t1.unique1 < 1;
-- (A parameterized scan of the left side runs once per unique-ified row:
-- adjust_rowcount_for_semijoins.)
explain (costs off)
select * from a23_t a inner join a23_t b
  on a.hundred in (select c.hundred from a23_t c where c.odd = b.odd)
  where a.thousand < 750 and b.unique1 < 3;
select count(*) from a23_t a inner join a23_t b
  on a.hundred in (select c.hundred from a23_t c where c.odd = b.odd)
  where a.thousand < 750 and b.unique1 < 3;
-- (The right side a join of several relations.)
create table a23_s2 (id serial, a int);
create table a23_s3 (id serial, a int);
create table a23_s4 (id serial, a int);
create unique index on a23_s3 (a, id);
create unique index on a23_s2 (id);
explain (costs off)
select a23_s2.id from a23_s2
where exists (select true from a23_s3, a23_s4 where a23_s3.a = 1 and a23_s3.id = a23_s2.id);
insert into a23_s2 (a) select i from generate_series(1, 5) i;
insert into a23_s3 (a) select 1 from generate_series(1, 3) i;
insert into a23_s4 (a) values (1), (2);
select a23_s2.id from a23_s2
where exists (select true from a23_s3, a23_s4 where a23_s3.a = 1 and a23_s3.id = a23_s2.id)
order by 1;

-- A join's columns in its relation's target list order: the first pair's relations, the
-- earlier one first, whichever is the outer side.
explain (verbose, costs off)
select t.hundred, q.q2 from a23_t t, a23_f f, a23_q q where t.hundred = f.f1 and t.thousand = q.q1;
drop table a23_t, a23_o, a23_q, a23_f, a23_inner, a23_fk, a23_s2, a23_s3, a23_s4;
