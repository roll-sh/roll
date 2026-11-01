-- Hash Join past hash_mem (agent 150): nodeHash's batches (the table doubling its batches,
-- or its space first, as rows come; later batches loaded from their files), with EXPLAIN
-- ANALYZE's Buckets / Batches / Memory Usage, and the rows in the order the batches give.
create table a150a (id int, t text);
create table a150b (id int, v text);
insert into a150a select g, md5(g::text) from generate_series(1, 20000) g;
insert into a150b select g % 15000, md5((g * 7)::text) from generate_series(1, 30000) g;
insert into a150b select null, 'n' || g from generate_series(1, 20) g;
analyze a150a, a150b;
set max_parallel_workers_per_gather = 0;
set enable_mergejoin = off;
set enable_nestloop = off;
create function a150_explain(q text) returns setof text language plpgsql as $$
declare ln text;
begin
  for ln in execute 'explain (analyze, costs off, timing off, summary off, buffers off) ' || q loop
    return next ln;
  end loop;
end $$;
-- One batch at the default work_mem.
select a150_explain('select * from a150a join a150b using (id)');
select md5(string_agg(id || t || v, ',')) from (select * from a150a join a150b using (id)) s;
set work_mem = '64kB';
select a150_explain('select * from a150a join a150b using (id)');
select md5(string_agg(id || t || v, ',')) from (select * from a150a join a150b using (id)) s;
select * from a150a join a150b using (id) limit 5;
select a150_explain('select * from a150b left join a150a using (id)');
select md5(string_agg(coalesce(id, -1) || coalesce(t, '-') || v, ',')) from (select * from a150b left join a150a using (id)) s;
select a150_explain('select * from a150a left join a150b using (id)');
select md5(string_agg(id || t || coalesce(v, '-'), ',')) from (select * from a150a left join a150b using (id)) s;
select a150_explain('select * from a150a full join a150b using (id)');
select md5(string_agg(coalesce(id, -1) || coalesce(t, '-') || coalesce(v, '-'), ',')) from (select * from a150a full join a150b using (id)) s;
select a150_explain('select * from a150a where exists (select 1 from a150b where a150b.id = a150a.id)');
select md5(string_agg(id || t, ',')) from (select * from a150a where exists (select 1 from a150b where a150b.id = a150a.id)) s;
select a150_explain('select * from a150a where not exists (select 1 from a150b where a150b.id = a150a.id)');
select md5(string_agg(id || t, ',')) from (select * from a150a where not exists (select 1 from a150b where a150b.id = a150a.id)) s;
-- Only the key column kept: smaller tuples.
select a150_explain('select a150b.v from a150a join a150b using (id)');
select md5(string_agg(v, ',')) from (select a150b.v from a150a join a150b using (id)) s;
-- An outer side with a most common value (the skew optimization's MCV buckets).
create table a150c (k int, p text);
insert into a150c select case when g <= 200 then 7 else g end, repeat('x', 40) from generate_series(1, 9000) g;
analyze a150c;
-- The outer side's most common value: its rows meet the skew table's (batch 0's) inner rows.
select a150_explain('select count(*) from a150b join a150c on a150c.k = a150b.id');
select md5(string_agg(id || ':' || p || v, ',')) from (select a150b.id, a150b.v, a150c.p from a150b join a150c on a150c.k = a150b.id) s;
-- Many inner rows of the most common value: its skew bucket outgrows its share and goes
-- to the main table.
create table a150e (k int, w text);
insert into a150e select case when g % 3 = 0 then 7 else g end, md5(g::text) from generate_series(1, 12000) g;
analyze a150e;
select a150_explain('select * from a150c join a150e on a150e.k = a150c.k');
select md5(string_agg(k || p || w, ',')) from (select a150c.k, a150c.p, a150e.w from a150c join a150e on a150e.k = a150c.k) s;
select a150_explain('select * from a150c left join a150e on a150e.k = a150c.k');
select md5(string_agg(k || p || coalesce(w, '-'), ',')) from (select a150c.k, a150c.p, a150e.w from a150c left join a150e on a150e.k = a150c.k) s;
set work_mem = '256kB';
select a150_explain('select * from a150a join a150b using (id)');
select md5(string_agg(id || t || v, ',')) from (select * from a150a join a150b using (id)) s;
select a150_explain('select * from a150b right join a150a using (id)');
select md5(string_agg(id || t || coalesce(v, '-'), ',')) from (select * from a150b right join a150a using (id)) s;
set work_mem = '1MB';
select a150_explain('select * from a150a join a150b using (id)');
select md5(string_agg(id || t || v, ',')) from (select * from a150a join a150b using (id)) s;
set hash_mem_multiplier = 1;
select a150_explain('select * from a150a join a150b using (id)');
select md5(string_agg(id || t || v, ',')) from (select * from a150a join a150b using (id)) s;
reset hash_mem_multiplier;
-- An estimate too low (a third of the rows): the batches grow while the table is built.
set work_mem = '64kB';
select a150_explain('select * from a150b join (select * from a150a where id % 7 < 100) a using (id)');
select md5(string_agg(id || t || v, ',')) from (select * from a150b join (select * from a150a where id % 7 < 100) a using (id)) s;
set work_mem = '512kB';
select a150_explain('select * from a150b join (select * from a150a where id % 7 < 100) a using (id)');
select md5(string_agg(id || t || v, ',')) from (select * from a150b join (select * from a150a where id % 7 < 100) a using (id)) s;
set work_mem = '64kB';
-- temp_file_limit counts the batch files.
set temp_file_limit = '100kB';
select count(*) from a150a join a150b using (id);
reset temp_file_limit;
reset work_mem;
drop table a150a, a150b, a150c, a150e;
drop function a150_explain(text);
