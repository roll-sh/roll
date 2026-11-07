-- Materialized executor nodes must not be rerun when a cursor moves backward.
create function a165_notice() returns setof int stable language plpgsql as $$
begin
  raise notice 'stable function ran';
  return next 1;
  return next 2;
end $$;
begin;
declare f scroll cursor for select * from a165_notice();
fetch all from f;
fetch backward all from f;
fetch all from f;
close f;
declare d cursor for select * from a165_notice();
fetch all from d;
fetch backward all from d;
rollback;
begin;
set local application_name = 'cursor opened';
declare s scroll cursor for select * from current_setting('application_name');
fetch all from s;
set local application_name = 'cursor rewound';
fetch backward all from s;
rollback;
-- Sort and materialized CTE inputs keep their stable values too.
begin;
set local application_name = 'sort opened';
declare s scroll cursor for select current_setting('application_name') as v from generate_series(1, 2) order by v;
fetch all from s;
set local application_name = 'sort rewound';
fetch backward all from s;
rollback;
begin;
set local application_name = 'cte opened';
declare s scroll cursor for with x as materialized (select * from current_setting('application_name')) select * from x;
fetch all from s;
set local application_name = 'cte rewound';
fetch backward all from s;
rollback;
-- FETCH statements have different statement timestamps; FunctionScan keeps its first row.
create table a165_times (t timestamptz);
begin;
declare ts scroll cursor for select * from statement_timestamp();
do $$ declare c refcursor := 'ts'; a timestamptz; begin fetch next from c into a; insert into a165_times values (a); end $$;
select pg_sleep(0.01);
do $$ declare c refcursor := 'ts'; b timestamptz; a timestamptz; begin
  fetch next from c into b;
  fetch prior from c into b;
  select t into a from a165_times;
  raise notice 'timestamp retained: %', a = b;
end $$;
rollback;
drop table a165_times;
create role a165_cursor_role;
create function a165_user() returns setof text stable language plpgsql as $$
begin return next current_user::text; end $$;
begin;
declare u scroll cursor for select * from a165_user();
fetch all from u;
set role a165_cursor_role;
fetch backward all from u;
reset role;
rollback;
drop function a165_user();
drop role a165_cursor_role;
-- At commit a WITH HOLD cursor reuses materialized function rows.
begin;
declare h scroll cursor with hold for select * from a165_notice();
fetch 1 from h;
commit;
fetch backward all from h;
fetch all from h;
close h;
drop function a165_notice();
