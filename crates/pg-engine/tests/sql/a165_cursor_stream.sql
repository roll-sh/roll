-- Cursors stream their rows (a165): a NO SCROLL cursor keeps none it passed and can't go back;
-- a scrollable immutable scan may restart once for a backward fetch, then keeps its rows;
-- materialized function/sort/CTE nodes keep rows from their first execution. Holdable cursors
-- keep their rows at commit.
create table a165_cs (a int primary key, b text);
insert into a165_cs select g, 'r' || g from generate_series(1, 50) g;
begin;
declare n no scroll cursor for select a, b from a165_cs order by a;
fetch 3 from n;
fetch next from n;
move forward 10 in n;
fetch 2 from n;
fetch prior from n;
rollback;
begin;
declare n no scroll cursor for select a from a165_cs where a > 45;
fetch all from n;
fetch next from n;
move backward all in n;
rollback;
begin;
declare n no scroll cursor for select a from a165_cs;
fetch 2 from n;
fetch relative 0 from n;
rollback;
begin;
declare n no scroll cursor for select a from a165_cs;
fetch absolute 3 from n;
fetch absolute 5 from n;
fetch absolute 2 from n;
rollback;
-- Default (scrollable plan): forward, back, rewind, absolute, relative, last.
begin;
declare s cursor for select a, b from a165_cs where a <= 10;
fetch 3 from s;
fetch prior from s;
fetch relative 0 from s;
fetch absolute 7 from s;
fetch relative -3 from s;
move backward all in s;
fetch next from s;
fetch last from s;
fetch backward 2 from s;
fetch absolute -1 from s;
fetch absolute 0 from s;
fetch forward all from s;
fetch backward all from s;
close s;
declare s2 scroll cursor for select a from a165_cs where a between 20 and 24 order by a desc;
fetch all from s2;
fetch first from s2;
fetch absolute 4 from s2;
move absolute 0 in s2;
fetch 2 from s2;
commit;
-- WHERE CURRENT OF on a streaming cursor, after moving back.
begin;
declare u cursor for select a, b from a165_cs where a <= 5;
fetch 3 from u;
fetch prior from u;
update a165_cs set b = 'upd' where current of u;
fetch next from u;
delete from a165_cs where current of u;
rollback;
-- A holdable cursor keeps its rows at commit (SCROLL: all of them).
begin;
declare h scroll cursor with hold for select a from a165_cs where a <= 6;
fetch 2 from h;
commit;
fetch next from h;
fetch first from h;
fetch last from h;
fetch absolute 3 from h;
close h;
begin;
declare h2 no scroll cursor with hold for select a from a165_cs where a <= 6;
fetch 2 from h2;
commit;
fetch all from h2;
fetch prior from h2;
close h2;
-- A volatile target keeps the rows under it (evaluated again when passed).
begin;
declare v scroll cursor for select a, random() < 2 as r from a165_cs where a <= 4;
fetch 3 from v;
fetch prior from v;
fetch first from v;
rollback;
-- The function form (PL/pgSQL FOR over a cursor, refcursor fetches).
create function a165_cs_sum() returns bigint language plpgsql as $$
declare c cursor for select a from a165_cs; t bigint := 0; r record;
begin
  for r in c loop t := t + r.a; end loop;
  return t;
end $$;
select a165_cs_sum();
do $$
declare c scroll cursor for select a from a165_cs where a <= 8; x int;
begin
  open c;
  fetch last from c into x; raise notice 'last %', x;
  fetch prior from c into x; raise notice 'prior %', x;
  move backward all from c;
  fetch next from c into x; raise notice 'first %', x;
  close c;
end $$;
drop function a165_cs_sum();
drop table a165_cs;
