create table shut (id int, t text);
insert into shut select g, md5(g::text) from generate_series(1, 12000) g;
analyze shut;
set max_parallel_workers_per_gather = 0;
set enable_mergejoin = off;
set enable_nestloop = off;
set work_mem = '64kB';
set temp_file_limit = '224kB';
select a.id from shut a join shut b on a.id = b.id limit 1;
select count(*) from (select a.id from shut a join shut b on a.id = b.id limit 1) s;
select * from (select a.id from shut a join shut b on a.id = b.id limit 1) s order by id;
select * from ((select a.id from shut a join shut b on a.id = b.id limit 1) union all select 99999) s limit 1;
with c as materialized (select a.id from shut a join shut b on a.id = b.id) select id from c limit 1;
select a.id from shut a join (select x.id from shut x join shut y on x.id = y.id limit 1) b on a.id = b.id limit 1;
select 1 / (n - 1)::int from (select count(*) n from (select a.id from shut a join shut b on a.id = b.id limit 1) s) t;
select 1 / (a.id - 14) from shut a join shut b on a.id = b.id limit 1;
begin;
declare c cursor for select a.id from shut a join shut b on a.id = b.id limit 1;
fetch all c;
close c;
rollback;
begin;
declare c cursor for select a.id from shut a join shut b on a.id = b.id limit 1;
fetch 1 c;
close c;
rollback;
begin;
declare c cursor for select a.id from shut a join shut b on a.id = b.id limit 1;
fetch 1 c;
commit;
begin;
declare c cursor with hold for select a.id from shut a join shut b on a.id = b.id limit 1;
fetch 1 c;
commit;
create function shut_catch() returns int language plpgsql as $$
begin
  perform 1 / (n - 1)::int from (select count(*) n from (select a.id from shut a join shut b on a.id = b.id limit 1) s) t;
  return 0;
exception when division_by_zero then return 42;
end
$$;
select shut_catch();
drop function shut_catch();
create function shut_after() returns int language plpgsql as $$
begin
  perform a.id from shut a join shut b on a.id = b.id limit 1;
  raise division_by_zero;
end
$$;
select shut_after();
drop function shut_after();
create function shut_sql() returns int language sql volatile as $$select a.id from shut a join shut b on a.id = b.id limit 1$$;
select 1 / (shut_sql() - 14);
drop function shut_sql();
reset temp_file_limit;
with c as materialized (select a.id from shut a join shut b on a.id = b.id) select count(*) from (select id from c limit 1) a, (select id from c offset 1 limit 1) b;
select a.id from shut a join shut b on a.id = b.id limit 1;
drop table shut;
