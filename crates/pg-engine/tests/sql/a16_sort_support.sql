-- Round 16 (agent 16): a sort by a made type's ordering operator whose btree family has no
-- comparison function (FUNCTION 1) for it errors as the sort starts (FinishSortSupportFunction),
-- as a merge join over an operator without a commutator does (equivclass's made types).
create type a16_int8alias1;
create function a16_int8alias1in(cstring) returns a16_int8alias1
  strict immutable language internal as 'int8in';
create function a16_int8alias1out(a16_int8alias1) returns cstring
  strict immutable language internal as 'int8out';
create type a16_int8alias1 (
    input = a16_int8alias1in,
    output = a16_int8alias1out,
    like = int8
);
create cast (int8 as a16_int8alias1) without function;
create cast (a16_int8alias1 as int8) without function;
create function a16_int8alias1eq(a16_int8alias1, a16_int8alias1) returns bool
  strict immutable language internal as 'int8eq';
create operator = (
    procedure = a16_int8alias1eq,
    leftarg = a16_int8alias1, rightarg = a16_int8alias1,
    commutator = =,
    restrict = eqsel, join = eqjoinsel,
    merges
);
alter operator family integer_ops using btree add
  operator 3 = (a16_int8alias1, a16_int8alias1);
create function a16_int8alias1eq(int8, a16_int8alias1) returns bool
  strict immutable language internal as 'int8eq';
create operator = (
    procedure = a16_int8alias1eq,
    leftarg = int8, rightarg = a16_int8alias1,
    restrict = eqsel, join = eqjoinsel,
    merges
);
alter operator family integer_ops using btree add
  operator 3 = (int8, a16_int8alias1);
create function a16_int8alias1lt(a16_int8alias1, a16_int8alias1) returns bool
  strict immutable language internal as 'int8lt';
create operator < (
    procedure = a16_int8alias1lt,
    leftarg = a16_int8alias1, rightarg = a16_int8alias1
);
alter operator family integer_ops using btree add
  operator 1 < (a16_int8alias1, a16_int8alias1);
create function a16_int8alias1cmp(int8, a16_int8alias1) returns int
  strict immutable language internal as 'btint8cmp';
alter operator family integer_ops using btree add
  function 1 a16_int8alias1cmp (int8, a16_int8alias1);

create table a16_nocom (a int8, b a16_int8alias1);
insert into a16_nocom select i, (i % 7)::int8::a16_int8alias1 from generate_series(1, 20) i;
insert into a16_nocom values (null, null), (100, null), (null, '3');
-- The type oid differs from run to run; the family is integer_ops.
create function a16_try(q text) returns text language plpgsql as $$
declare n bigint;
begin
  execute q into n;
  return n::text;
exception when others then
  return sqlstate || ': ' || regexp_replace(sqlerrm, '\(\d+,\d+\)', '(T,T)');
end $$;
-- Merge joins through the operator without a commutator sort b by the family's "<":
-- FinishSortSupportFunction finds no FUNCTION 1 (alias, alias) as the sort starts.
set enable_hashjoin to off;
set enable_mergejoin to on;
explain (costs off)
select * from a16_nocom t1 full join a16_nocom t2 on t2.a = t1.b;
select a16_try('select count(*) from a16_nocom t1 full join a16_nocom t2 on t2.a = t1.b');
select a16_try('select count(*) from a16_nocom t1 left join a16_nocom t2 on t2.a = t1.b');
select a16_try('select count(*) from a16_nocom t1 right join a16_nocom t2 on t2.a = t1.b');
-- The operator's other order doesn't exist.
select * from a16_nocom t1 full join a16_nocom t2 on t1.b = t2.a;
-- ORDER BY ... USING the family's "<" sorts the same way.
select a16_try('select count(*) from (select b from a16_nocom order by b using <) s');
-- An output named like the column, cast to int8, sorts by int8's "<".
select b::int8 from a16_nocom order by b using < limit 3;
-- A nested loop needs no sort.
set enable_mergejoin to off;
select count(*) from a16_nocom t1 full join a16_nocom t2 on t2.a = t1.b and t1.a < 5;
select count(*) from a16_nocom t1 left join a16_nocom t2 on t2.a = t1.b;
-- With the comparison function the merge join runs.
create function a16_int8alias1cmp(a16_int8alias1, a16_int8alias1) returns int
  strict immutable language internal as 'btint8cmp';
alter operator family integer_ops using btree add
  function 1 a16_int8alias1cmp (a16_int8alias1, a16_int8alias1);
set enable_mergejoin to on;
select a16_try('select count(*) from a16_nocom t1 full join a16_nocom t2 on t2.a = t1.b');
select t1.a, t1.b::int8, t2.a, t2.b::int8 from a16_nocom t1 full join a16_nocom t2 on t2.a = t1.b
  order by 1, 2, 3, 4;
reset enable_mergejoin;
reset enable_hashjoin;
drop table a16_nocom;
drop function a16_try(text);

-- A FULL JOIN with no merge- or hash-joinable plan (populate_joinrel_with_paths).
create table a16_fj (a int, b int);
insert into a16_fj select i, i % 3 from generate_series(1, 10) i;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on x.a < y.a;
select count(*) from a16_fj x full join a16_fj y on x.a < y.a;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on x.a = y.a and x.b < y.b;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on x.a = y.a and x.b < 2;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on x.a = y.a or x.b = y.b;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on x.a + y.a = 3;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on true;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on false;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on x.a = 1;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on x.a = y.a and random() > 0.5;
explain (costs off) select count(*) from a16_fj x full join (select a, 1 as c from a16_fj) y on x.a = y.c;
explain (costs off) select count(*) from a16_fj x full join lateral (select x.a as c) y on true;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on (x.a, x.b) = (y.a, y.b);
explain (costs off) select count(*) from a16_fj x full join a16_fj y on x.a is not distinct from y.a;
explain (costs off) select count(*) from a16_fj x full join a16_fj y on x.a < y.a and false;
select count(*) from a16_fj x full join a16_fj y on x.a = y.a and x.b < y.b;
drop table a16_fj;
