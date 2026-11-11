-- Batch boundaries must preserve NULLs, residual join filters, mixed integer key types,
-- duplicate matches, and semi/anti joins whose hashed side is the returned side.
create table a150l (id int, k text, v int);
create table a150r (id bigint, k text, v int);
insert into a150l select case when g % 100 = 0 then null else g % 6000 end, case when g % 73 = 0 then null else (g % 3)::text end, g from generate_series(1, 8000) g;
insert into a150r select case when g % 101 = 0 then null else g % 6000 end, case when g % 71 = 0 then null else (g % 3)::text end, g from generate_series(1, 16000) g;
analyze a150l, a150r;
set max_parallel_workers_per_gather = 0;
set enable_mergejoin = off;
set enable_nestloop = off;
set work_mem = '64kB';
select count(*), sum(l.v), sum(r.v) from a150l l join a150r r on l.id = r.id and l.k = r.k and l.v < r.v;
select count(*), sum(l.v), sum(r.v) from a150l l left join a150r r on l.id = r.id and l.k = r.k and l.v > r.v;
select count(*), sum(l.v), sum(r.v) from a150l l right join a150r r on l.id = r.id and l.k = r.k and l.v > r.v;
select count(*), sum(l.v), sum(r.v) from a150l l full join a150r r on l.id = r.id and l.k = r.k and l.v > r.v;
select count(*), sum(l.v) from a150l l where exists (select 1 from a150r r where l.id = r.id and l.k = r.k and l.v < r.v);
select count(*), sum(l.v) from a150l l where not exists (select 1 from a150r r where l.id = r.id and l.k = r.k and l.v < r.v);
-- Reversing sizes makes the planner hash the semi/anti join's returned side.
select count(*), sum(r.v) from a150r r where exists (select 1 from a150l l where l.id = r.id and l.k = r.k and l.v < r.v);
select count(*), sum(r.v) from a150r r where not exists (select 1 from a150l l where l.id = r.id and l.k = r.k and l.v < r.v);
select count(*) from a150l l full join (select * from a150r where v < 0) r on l.id = r.id;
select count(*) from (select * from a150l where v < 0) l full join a150r r on l.id = r.id;
reset work_mem;
drop table a150l, a150r;
