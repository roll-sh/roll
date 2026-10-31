-- Sorts past work_mem (agent 132): tuplesort's method and space at several work_mem
-- values, and the rows' order (equal keys included) when runs on disk are merged.
-- (Disk sizes of sorts merged in several passes filtered: the engine counts the runs'
-- blocks, PostgreSQL also a few its intermediate merges add.)
create function a132_explain(q text) returns setof text language plpgsql as $$
declare ln text;
begin
  for ln in execute 'explain (analyze, costs off, timing off, summary off, buffers off) ' || q loop
    return next regexp_replace(ln, 'Disk: \d+kB', 'Disk: NkB');
  end loop;
end $$;
create table a132s (a int, t text, n numeric);
insert into a132s select g, md5(g::text), (g % 97) * 1.5 from generate_series(1, 30000) g;
create table a132i (a int);
insert into a132i select (g * 7919) % 30011 from generate_series(1, 30000) g;
analyze a132s;
analyze a132i;
set max_parallel_workers_per_gather = 0;
-- In memory.
explain (analyze, costs off, timing off, summary off, buffers off) select * from a132s order by t;
explain (analyze, costs off, timing off, summary off, buffers off) select a from a132i order by a;
explain (analyze, costs off, timing off, summary off, buffers off) select * from a132s order by t limit 10;
set work_mem = '64kB';
select a132_explain('select * from a132s order by t');
explain (analyze, costs off, timing off, summary off, buffers off) select a from a132i order by a;
select a132_explain('select * from a132s order by n, a desc');
-- Equal keys: the merge's order.
select md5(string_agg(a::text, ',')) from (select a from a132s order by n) s;
select md5(string_agg(a::text, ',')) from (select a, t from a132s order by a % 10) s;
select md5(string_agg(t, ',')) from (select t from a132s order by left(t, 1)) s;
set work_mem = '256kB';
explain (analyze, costs off, timing off, summary off, buffers off) select * from a132s order by t;
select md5(string_agg(a::text, ',')) from (select a from a132s order by n) s;
select md5(string_agg(a::text, ',')) from (select a, t from a132s order by a % 10) s;
set work_mem = '1MB';
explain (analyze, costs off, timing off, summary off, buffers off) select * from a132s order by t;
explain (analyze, costs off, timing off, summary off, buffers off) select * from a132s order by n;
select md5(string_agg(a::text, ',')) from (select a from a132s order by n) s;
set work_mem = '2MB';
explain (analyze, costs off, timing off, summary off, buffers off) select * from a132s order by t;
explain (analyze, costs off, timing off, summary off, buffers off) select a from a132i order by a;
select count(*), sum(a) from (select * from a132s order by t) s;
reset work_mem;
-- temp_file_limit: an error once the sort's files would pass it.
set work_mem = '64kB';
set temp_file_limit = '512kB';
select count(*) from (select * from a132s order by t) s;
reset temp_file_limit;
reset work_mem;
drop table a132s, a132i;
drop function a132_explain(text);
