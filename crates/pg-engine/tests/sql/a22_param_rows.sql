-- A parameterized join relation's rows are its ParamPathInfo's, estimated from the first
-- pair of input paths that made a path of it (get_joinrel_parampathinfo); an index's
-- parameterized paths are of the outer relations its own clauses read
-- (consider_index_join_clauses).
create table ppr_t (unique1 int, unique2 int, thousand int, tenthous int, filler text);
insert into ppr_t select i, (i * 7919) % 10000, i % 1000, i, repeat($$x$$, 200) from generate_series(0, 9999) i;
create index ppr_t_unique1 on ppr_t (unique1);
create index ppr_t_unique2 on ppr_t (unique2);
create index ppr_t_thous_tenthous on ppr_t (thousand, tenthous);
create table ppr_i (f1 int);
insert into ppr_i values (0), (123456), (-123456), (2147483647), (-2147483647);
analyze ppr_t, ppr_i;
explain (costs off)
select b.unique1 from
  ppr_t a join ppr_t b on a.unique1 = b.unique2
  left join ppr_t c on b.unique1 = 42 and c.thousand = a.thousand
  join ppr_i i1 on b.thousand = f1
  right join ppr_i i2 on i2.f1 = b.tenthous
  order by 1;
select b.unique1 from
  ppr_t a join ppr_t b on a.unique1 = b.unique2
  left join ppr_t c on b.unique1 = 42 and c.thousand = a.thousand
  join ppr_i i1 on b.thousand = f1
  right join ppr_i i2 on i2.f1 = b.tenthous
  order by 1;
insert into ppr_i values (5), (7);
select b.unique1, b.tenthous from
  ppr_t a join ppr_t b on a.unique1 = b.unique2
  left join ppr_t c on b.unique1 = 42 and c.thousand = a.thousand
  join ppr_i i1 on b.thousand = f1
  right join ppr_i i2 on i2.f1 = b.tenthous
  order by 1, 2;
drop table ppr_t, ppr_i;
