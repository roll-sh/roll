-- An inner join's `name = text` clause out of its equivalence class: the text side relabelled
-- to the comparison's collation (name's C) shows as a cast; an outer join's clause, a
-- sub-select's value and a constant don't (subselect 328's shape).
create temp table er_nk (u int, s name);
insert into er_nk select g, 'A' || g from generate_series(1, 2000) g;
create index er_nk_s on er_nk (s);
create temp table er_tx (i text);
insert into er_tx values ('A1'), ('A2');
analyze er_nk;
analyze er_tx;

explain (costs off) select * from er_nk, (values ('A5'), ('A9')) as v(i) where er_nk.s = v.i;
explain (costs off) select * from er_nk, er_tx where er_nk.s = er_tx.i;
explain (costs off) select * from er_nk, (values ('A5'::name), ('A9')) as v(i) where er_nk.s = v.i;
explain (costs off) select * from er_nk, (values ('A5'::varchar), ('A9')) as v(i) where er_nk.s = v.i;
explain (costs off) select * from er_nk where er_nk.s = (select i from er_tx limit 1);
explain (costs off) select * from er_nk, er_tx where er_nk.s = upper(er_tx.i);
explain (costs off) select * from er_tx left join er_nk on er_nk.s = er_tx.i;
explain (costs off) select * from er_nk, er_tx where er_nk.s = er_tx.i and er_nk.u < 5;
select * from er_nk, er_tx where er_nk.s = er_tx.i order by 1;
select * from er_tx left join er_nk on er_nk.s = er_tx.i || '0' order by 1;

-- A semi join's side unique-ified on an expression (create_unique_path over semi_rhs_exprs,
-- subselect 332's shape): sorted on it, a Unique keeping one row per value.
create temp table er_ok (unique1 int, two int);
insert into er_ok select g, g % 2 from generate_series(0, 999) g;
create unique index er_ok_u1 on er_ok (unique1);
analyze er_ok;

explain (costs off)
select * from er_ok where unique1 in (select x * x from (values (1200), (1)) as x(x));
select * from er_ok where unique1 in (select x * x from (values (1200), (1)) as x(x));
select * from er_ok where unique1 in (select x * x from (values (2), (-2), (3), (3), (-3), (30)) as x(x))
order by 1;
select count(*) from er_ok a where unique1 in (select b.unique1 % 10 from er_ok b where b.two = 1);
select * from er_ok where (unique1, two) in (select x * x, y from (values (2, 0), (-2, 0), (3, 1), (3, 0)) as x(x, y))
order by 1;
