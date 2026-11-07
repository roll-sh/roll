create function a165_scalar() returns int stable language plpgsql as $$
begin raise notice 'scalar from'; return 7; end $$;
explain (costs off) declare s cursor for select * from a165_scalar();
begin;
declare s cursor for select * from a165_scalar();
select name, is_scrollable from pg_cursors where name = 's';
fetch all from s;
fetch prior from s;
fetch backward all from s;
rollback;
drop function a165_scalar();
