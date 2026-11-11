-- An equivalence class's join clause keeps the operand order of its first making
-- (ec_derives): here a parameterized join path's (get_joinrel_parampathinfo's dropped
-- class, the parameters' member first), reused by the join of the two subqueries.
create table ecd_t (thousand int, tenthous int, filler text);
insert into ecd_t select i % 1000, i, 'x' from generate_series(0, 9999) i;
create index ecd_t_thous_tenthous on ecd_t (thousand, tenthous);
create table ecd_i (f1 int);
insert into ecd_i values (0), (123456), (-123456), (2147483647), (-2147483647);
analyze ecd_t, ecd_i;
explain (costs off)
select a.f1, b.f1, t.thousand, t.tenthous from
  ecd_t t,
  (select sum(f1)+1 as f1 from ecd_i i4a) a,
  (select sum(f1) as f1 from ecd_i i4b) b
where b.f1 = t.thousand and a.f1 = b.f1 and (a.f1+b.f1+999) = t.tenthous;
select a.f1, b.f1, t.thousand, t.tenthous from
  ecd_t t,
  (select sum(f1)+1 as f1 from ecd_i i4a) a,
  (select sum(f1) as f1 from ecd_i i4b) b
where b.f1 = t.thousand and a.f1 = b.f1 and (a.f1+b.f1+999) = t.tenthous;
update ecd_i set f1 = 1 where f1 = 0;
select a.f1, b.f1, t.thousand, t.tenthous from
  ecd_t t,
  (select max(f1) as f1 from ecd_i i4a where f1 between 0 and 5) a,
  (select max(f1) as f1 from ecd_i i4b where f1 between 0 and 5) b
where b.f1 = t.thousand and a.f1 = b.f1 and (a.f1+b.f1+999) = t.tenthous;
drop table ecd_t, ecd_i;
