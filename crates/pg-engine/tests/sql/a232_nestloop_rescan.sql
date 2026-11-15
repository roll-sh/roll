-- Agent 232: correlated SubPlans whose trees are nested loops (TPC-H q02's shape: a
-- sequential scan of a small table on the outer side, an unparameterized inner side that
-- reads the enclosing row, parameterized index scans below it), kept and rescanned for each
-- outer row; and an Aggregate over a bitmap heap scan (q17's), whose scan is opened again
-- for each. Every outer row's result is its own, inner, left, semi and anti joins alike.
create table a232_r (rk int, name text);
insert into a232_r values (0, 'AF'), (1, 'AM'), (2, 'AS'), (3, 'EU'), (4, 'ME');
create table a232_n (nk int primary key, rk int);
insert into a232_n select g, g % 5 from generate_series(0, 24) g;
create table a232_s (sk int primary key, nk int);
insert into a232_s select g, (g * 7) % 25 from generate_series(1, 200) g;
create table a232_ps (pk int, sk int, cost int, primary key (pk, sk));
insert into a232_ps select p, 1 + (p * 13 + i * 50) % 200, (p * 31 + i * 17) % 997 from generate_series(1, 300) p, generate_series(0, 3) i;
create table a232_p (pk int, sz int);
insert into a232_p select g, g % 9 from generate_series(1, 300) g;
analyze a232_r, a232_n, a232_s, a232_ps, a232_p;
set enable_hashjoin = off;
set enable_mergejoin = off;
set enable_material = off;
set enable_memoize = off;
set max_parallel_workers_per_gather = 0;
explain (costs off)
select p.pk, (select min(ps.cost) from a232_ps ps, a232_s s, a232_n n, a232_r r
               where ps.pk = p.pk and s.sk = ps.sk and n.nk = s.nk and n.rk = r.rk and r.name = 'EU')
  from a232_p p where p.sz = 3;
select p.pk, (select min(ps.cost) from a232_ps ps, a232_s s, a232_n n, a232_r r
               where ps.pk = p.pk and s.sk = ps.sk and n.nk = s.nk and n.rk = r.rk and r.name = 'EU') as m
  from a232_p p where p.sz = 3 order by 1;
select count(*), sum(m), count(m) from (
  select (select min(ps.cost) from a232_ps ps, a232_s s, a232_n n, a232_r r
           where ps.pk = p.pk and s.sk = ps.sk and n.nk = s.nk and n.rk = r.rk and r.name in ('EU', 'AS')) as m
    from a232_p p) t;
-- A left join, and semi / anti joins, inside the SubPlan.
select p.pk, (select count(n.nk) from a232_r r left join a232_n n on n.rk = r.rk and n.nk = p.pk % 30) as c
  from a232_p p where p.pk <= 40 order by 1;
select p.pk, (select count(*) from a232_r r where exists (select 1 from a232_n n where n.rk = r.rk and n.nk = p.pk % 30)) as c
  from a232_p p where p.pk <= 40 order by 1;
select p.pk, (select count(*) from a232_r r where not exists (select 1 from a232_n n where n.rk = r.rk and n.nk = p.pk % 30)) as c
  from a232_p p where p.pk <= 40 order by 1;
-- The SubPlan compared in a hash join's condition, as q02 has it.
reset enable_hashjoin;
select p.pk, ps.sk, ps.cost from a232_p p, a232_ps ps
  where p.pk = ps.pk and p.sz = 5
    and ps.cost = (select min(ps2.cost) from a232_ps ps2, a232_s s, a232_n n, a232_r r
                    where ps2.pk = p.pk and s.sk = ps2.sk and n.nk = s.nk and n.rk = r.rk and r.name = 'EU')
  order by 1, 2;
-- An Aggregate over a bitmap heap scan of the enclosing row's key.
set enable_hashjoin = off;
set enable_indexscan = off;
explain (costs off)
select p.pk, (select avg(ps.cost) from a232_ps ps where ps.pk = p.pk) from a232_p p where p.sz = 1;
select p.pk, (select avg(ps.cost) from a232_ps ps where ps.pk = p.pk) as a, (select count(*) from a232_ps ps where ps.pk = p.pk + 1000) as none
  from a232_p p where p.sz = 1 order by 1;
reset enable_indexscan;
reset enable_hashjoin;
reset enable_mergejoin;
reset enable_material;
reset enable_memoize;
reset max_parallel_workers_per_gather;
