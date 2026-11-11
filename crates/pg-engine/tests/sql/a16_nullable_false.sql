-- Round 16 (agent 16): a pulled-up subquery's WHERE false on an outer join's nullable side
-- goes to the front of the join's ON clause (remove_useless_results_recurse), its
-- relation a dummy rel (join 772, 777).
create table a16_i8 (q1 int8, q2 int8);
create table a16_i4 (f1 int4);
insert into a16_i8 values (123, 456), (123, 4567890123456789), (4567890123456789, 123), (4567890123456789, 4567890123456789), (4567890123456789, -4567890123456789);
insert into a16_i4 values (0), (123456), (-123456), (2147483647), (-2147483647);
analyze a16_i8; analyze a16_i4;
explain (verbose, costs off)
select * from
  (select 0 as val0) as ss0
  left join (select 1 as val) as ss1 on true
  left join lateral (select ss1.val as val_filtered where false) as ss2 on true;
select * from
  (select 0 as val0) as ss0
  left join (select 1 as val) as ss1 on true
  left join lateral (select ss1.val as val_filtered where false) as ss2 on true;
explain (verbose, costs off)
select * from a16_i8 i8 left join lateral
  (select *, i8.q2 from a16_i4 where false) ss on true;
explain (verbose, costs off)
select * from a16_i8 i8 left join lateral
  (select *, i8.q2 from a16_i4 i1, a16_i4 i2 where false) ss on true;
explain (verbose, costs off)
select * from a16_i8 a left join (select * from a16_i4 where false) ss on ss.f1 = a.q1;
select count(*) from a16_i8 a left join (select * from a16_i4 where false) ss on ss.f1 = a.q1;
explain (verbose, costs off)
select * from a16_i8 a left join (select * from a16_i4 where false) ss on ss.f1 = a.q1 and a.q2 > 0;
explain (verbose, costs off)
select * from (select * from a16_i4 where false) ss right join a16_i8 a on true;
drop table a16_i8;
drop table a16_i4;
