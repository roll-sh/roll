-- Round 15 (agent 10): equivalence classes over made int8 alias types (equivclass): broken
-- classes keep their constant's clause twice, a scan's own equalities carry a join column
-- into a class with a constant, Merge Appends of UNION ALL arms as merge join inputs, merge
-- and hash clauses of an operator without a commutator (FULL JOIN sides swapped).
create type a10_int8alias1;
create function a10_int8alias1in(cstring) returns a10_int8alias1
  strict immutable language internal as 'int8in';
create function a10_int8alias1out(a10_int8alias1) returns cstring
  strict immutable language internal as 'int8out';
create type a10_int8alias1 (
    input = a10_int8alias1in,
    output = a10_int8alias1out,
    like = int8
);
create cast (int8 as a10_int8alias1) without function;
create cast (a10_int8alias1 as int8) without function;
create function a10_int8alias1eq(a10_int8alias1, a10_int8alias1) returns bool
  strict immutable language internal as 'int8eq';
create operator = (
    procedure = a10_int8alias1eq,
    leftarg = a10_int8alias1, rightarg = a10_int8alias1,
    commutator = =,
    restrict = eqsel, join = eqjoinsel,
    merges
);
alter operator family integer_ops using btree add
  operator 3 = (a10_int8alias1, a10_int8alias1);
create function a10_int8alias1eq(int8, a10_int8alias1) returns bool
  strict immutable language internal as 'int8eq';
create operator = (
    procedure = a10_int8alias1eq,
    leftarg = int8, rightarg = a10_int8alias1,
    restrict = eqsel, join = eqjoinsel,
    merges
);
alter operator family integer_ops using btree add
  operator 3 = (int8, a10_int8alias1);
create function a10_int8alias1lt(a10_int8alias1, a10_int8alias1) returns bool
  strict immutable language internal as 'int8lt';
create operator < (
    procedure = a10_int8alias1lt,
    leftarg = a10_int8alias1, rightarg = a10_int8alias1
);
alter operator family integer_ops using btree add
  operator 1 < (a10_int8alias1, a10_int8alias1);
create function a10_int8alias1cmp(int8, a10_int8alias1) returns int
  strict immutable language internal as 'btint8cmp';
alter operator family integer_ops using btree add
  function 1 a10_int8alias1cmp (int8, a10_int8alias1);

create table a10_ec1 (ff int8 primary key, f1 a10_int8alias1, f2 int8);
create table a10_ec2 (xf int8 primary key, x1 a10_int8alias1, x2 int8);
set enable_hashjoin = off;
set enable_mergejoin = off;
-- A broken class: `ff = 42` derived and kept as written.
explain (costs off)
  select * from a10_ec1, a10_ec2 where ff = x1 and ff = '42'::int8;
create unique index a10_ec1_expr2 on a10_ec1((ff + 2 + 1));
create unique index a10_ec1_expr3 on a10_ec1((ff + 3 + 1));
create unique index a10_ec1_expr4 on a10_ec1((ff + 4));
-- The scan's `ff = f1` brings the join column into the constant's class.
explain (costs off)
  select * from a10_ec1,
    (select ff + 1 as x from
       (select ff + 2 as ff from a10_ec1
        union all
        select ff + 3 as ff from a10_ec1) ss0
     union all
     select ff + 4 as x from a10_ec1) as ss1
  where ss1.x = a10_ec1.f1 and a10_ec1.ff = 42::int8 and a10_ec1.ff = a10_ec1.f1;
-- The UNION ALL arms merged in the join's order (Merge Appends), as merge join inputs.
set enable_mergejoin = on;
set enable_nestloop = off;
explain (costs off)
  select * from a10_ec1,
    (select ff + 1 as x from
       (select ff + 2 as ff from a10_ec1
        union all
        select ff + 3 as ff from a10_ec1) ss0
     union all
     select ff + 4 as x from a10_ec1) as ss1,
    (select ff + 1 as x from
       (select ff + 2 as ff from a10_ec1
        union all
        select ff + 3 as ff from a10_ec1) ss0
     union all
     select ff + 4 as x from a10_ec1) as ss2
  where ss1.x = a10_ec1.f1 and ss1.x = ss2.x and a10_ec1.ff = 42::int8;
drop index a10_ec1_expr3;
explain (costs off)
  select * from a10_ec1,
    (select ff + 1 as x from
       (select ff + 2 as ff from a10_ec1
        union all
        select ff + 3 as ff from a10_ec1) ss0
     union all
     select ff + 4 as x from a10_ec1) as ss1
  where ss1.x = a10_ec1.f1 and a10_ec1.ff = 42::int8;
insert into a10_ec1 select i, (i % 10)::int8::a10_int8alias1, i from generate_series(1, 30) i;
select a10_ec1.ff, ss1.x from a10_ec1,
    (select ff + 1 as x from
       (select ff + 2 as ff from a10_ec1
        union all
        select ff + 3 as ff from a10_ec1) ss0
     union all
     select ff + 4 as x from a10_ec1) as ss1
  where ss1.x = a10_ec1.f1::int8 and a10_ec1.ff < 12::int8
  order by 1, 2;
reset enable_nestloop;
reset enable_mergejoin;
reset enable_hashjoin;

-- A merge or hash clause whose operator has no commutator: the outer side on its left.
create table a10_nocom (a int8, b a10_int8alias1);
insert into a10_nocom select i, (i % 7)::int8::a10_int8alias1 from generate_series(1, 20) i;
insert into a10_nocom values (null, null), (100, null), (null, '3');
set enable_hashjoin to off;
set enable_mergejoin to on;
explain (costs off)
select * from a10_nocom t1 full join a10_nocom t2 on t2.a = t1.b;
alter operator = (int8, a10_int8alias1) set (hashes);
alter operator family integer_ops using hash add
  operator 1 = (int8, a10_int8alias1);
create function a10_hashint8alias1(a10_int8alias1) returns int
  strict immutable language internal as 'hashint8';
alter operator family integer_ops using hash add
  function 1 a10_hashint8alias1(a10_int8alias1);
set enable_hashjoin to on;
set enable_mergejoin to off;
explain (costs off)
select * from a10_nocom t1 full join a10_nocom t2 on t2.a = t1.b;
select t1.a, t1.b::int8, t2.a, t2.b::int8 from a10_nocom t1 full join a10_nocom t2 on t2.a = t1.b
  order by 1, 2, 3, 4;
-- An inner join's clause goes into its class, which is broken the other way round: no hash
-- join (and a disabled nested loop shows it).
explain (costs off)
select * from a10_nocom t1 join a10_nocom t2 on t2.a = t1.b;
set enable_nestloop to off;
explain (costs off)
select * from a10_nocom t1 join a10_nocom t2 on t2.a = t1.b;
select count(*) from a10_nocom t1 join a10_nocom t2 on t2.a = t1.b;
reset enable_nestloop;
reset enable_hashjoin;
reset enable_mergejoin;
drop table a10_nocom;
drop table a10_ec1;
drop table a10_ec2;
