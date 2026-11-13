-- Agent 227: an outer join's equality whose sides' restrictions equate both columns with
-- one constant (reconsider_outer_join_clauses): PostgreSQL replaces it with a constant TRUE,
-- so it parameterizes no scan of the nullable side, which reads its rows by the constant
-- alone (an anti join's inner scan had `(ok = o.ok) AND (ok = 5)`).
create table a227_ao (ok int primary key, st char(1));
create table a227_al (ok int, ln int, sk int, primary key (ok, ln));
insert into a227_ao select g, case when g % 2 = 0 then 'F' else 'O' end from generate_series(1, 7000) g;
insert into a227_al select o, ln, (o * 7 + ln * 13) % 1000 + 1 from generate_series(1, 7000) o, generate_series(1, 4) ln;
analyze a227_ao, a227_al;
set max_parallel_workers_per_gather = 0;
explain (costs off) select * from a227_ao o
  where o.ok = 5 and not exists (select 1 from a227_al l3 where l3.ok = o.ok and l3.sk = 1);
select * from a227_ao o
  where o.ok = 5 and not exists (select 1 from a227_al l3 where l3.ok = o.ok and l3.sk = 1);
select * from a227_ao o
  where o.ok = 6 and not exists (select 1 from a227_al l3 where l3.ok = o.ok and l3.sk = 56);
explain (costs off) select * from a227_ao o left join a227_al l3 on l3.ok = o.ok where o.ok = 5;
select * from a227_ao o left join a227_al l3 on l3.ok = o.ok where o.ok = 5 order by l3.ln;
-- The constant on the nullable side only: the clause stays, a parameterized scan may use it.
explain (costs off) select * from a227_ao o
  where o.st = 'F' and not exists (select 1 from a227_al l3 where l3.ok = o.ok and l3.ok = 6);
reset max_parallel_workers_per_gather;
