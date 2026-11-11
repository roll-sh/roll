-- HashAggregate past hash_mem (agent 132): nodeAgg's spill to partitions and its batches
-- in EXPLAIN ANALYZE (memory and disk sizes filtered: the engine's model of them is within
-- a block or two of PostgreSQL's), and the groups' results with batches.
create function a132_explain(q text) returns setof text language plpgsql as $$
declare ln text;
begin
  for ln in execute 'explain (analyze, costs off, timing off, summary off, buffers off) ' || q loop
    ln := regexp_replace(ln, '(Memory Usage|Disk Usage): \d+kB', '\1: NkB', 'g');
    return next ln;
  end loop;
end $$;
create table a132h (a int, b int, t text, n numeric);
insert into a132h select g, g % 5000, md5((g % 20000)::text), g % 1000 from generate_series(1, 40000) g;
analyze a132h;
set max_parallel_workers_per_gather = 0;
set enable_sort = off;
select a132_explain('select b, count(*) from a132h group by b');
set work_mem = '64kB';
select a132_explain('select b, count(*) from a132h group by b');
select a132_explain('select t, count(*), sum(a) from a132h group by t');
select a132_explain('select a, count(*) from a132h group by a');
-- The groups' results across batches.
select md5(string_agg(b::text || ':' || c, ',' order by b)) from (select b, count(*) c from a132h group by b) s;
select md5(string_agg(t || ':' || c || ':' || s, ',' order by t)) from (select t, count(*) c, sum(a) s from a132h group by t) s;
select count(*), sum(a) from (select a from a132h group by a) s;
select md5(string_agg(b::text || ':' || s || ':' || v, ',' order by b)) from (select b, sum(n) s, avg(a) v from a132h group by b) s;
select count(*) from (select a, b from a132h group by a, b) s;
set work_mem = '256kB';
select a132_explain('select t, count(*), sum(a) from a132h group by t');
select md5(string_agg(t || ':' || c, ',' order by t)) from (select t, count(*) c, sum(a) from a132h group by t) s;
set work_mem = '1MB';
select a132_explain('select t, count(*), sum(a) from a132h group by t');
select a132_explain('select a, count(*) from a132h group by a');
set hash_mem_multiplier = 1;
select a132_explain('select a, count(*) from a132h group by a');
reset hash_mem_multiplier;
reset work_mem;
select a132_explain('select t, count(*), sum(a) from a132h group by t');
reset enable_sort;
drop table a132h;
drop function a132_explain(text);
