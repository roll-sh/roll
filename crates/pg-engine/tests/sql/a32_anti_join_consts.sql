-- reconsider_outer_join_clauses for anti joins (left_join_clauses take an anti join's
-- clauses too): `outer = inner` whose outer column is equated with a constant gives the
-- anti join's inner side `inner = constant`, and the clause becomes constant TRUE.
-- reduce_outer_joins sees the left join's ON clause as written: a left join whose key a
-- constant replaced is still strict for its nullable column, so `IS NULL` above makes it
-- an anti join.
create table ajc_a (x int, v int);
create table ajc_b (y int, w int);
insert into ajc_a select g % 10, g from generate_series(1, 100) g;
insert into ajc_b select g % 7, g from generate_series(1, 100) g;
analyze ajc_a, ajc_b;
explain (costs off)
select * from ajc_a where ajc_a.x = 1 and not exists (select 1 from ajc_b where ajc_b.y = ajc_a.x);
select count(*) from ajc_a where ajc_a.x = 1 and not exists (select 1 from ajc_b where ajc_b.y = ajc_a.x);
select * from ajc_a where ajc_a.x = 8 and not exists (select 1 from ajc_b where ajc_b.y = ajc_a.x) order by v;
explain (costs off)
select * from ajc_a where ajc_a.x = 1 and not exists (select 1 from ajc_b where ajc_b.y = ajc_a.x and ajc_b.w > ajc_a.v);
select * from ajc_a where ajc_a.x = 1 and not exists (select 1 from ajc_b where ajc_b.y = ajc_a.x and ajc_b.w > ajc_a.v) order by v;
explain (costs off)
select * from ajc_a left join ajc_b on ajc_b.y = ajc_a.x where ajc_b.y is null;
explain (costs off)
select * from ajc_a left join ajc_b on ajc_b.y = ajc_a.x where ajc_a.x = 1 and ajc_b.y is null;
select * from ajc_a left join ajc_b on ajc_b.y = ajc_a.x where ajc_a.x = 8 and ajc_b.y is null order by v;
select count(*) from ajc_a left join ajc_b on ajc_b.y = ajc_a.x where ajc_a.x = 1 and ajc_b.y is null;
-- A multi-key EXISTS made a hashed ANY: each comparison of its test expression
-- parenthesized (an AND of OpExprs).
explain (costs off, verbose)
select * from ajc_a where exists (select 1 from ajc_b where ajc_b.y = ajc_a.x and ajc_b.w = ajc_a.v) or ajc_a.v < 0;
explain (costs off)
select * from ajc_a where not exists (select 1 from ajc_b where ajc_b.y = ajc_a.x and ajc_b.w + 1 = ajc_a.v + 1) or ajc_a.v < 0;
select count(*) from ajc_a where exists (select 1 from ajc_b where ajc_b.y = ajc_a.x and ajc_b.w = ajc_a.v) or ajc_a.v < 0;
drop table ajc_a;
drop table ajc_b;
