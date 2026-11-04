create table a165_edge (id int primary key, v text);
insert into a165_edge values (1, 'one'), (2, 'two'), (3, 'three');
-- The first backward fetch follows this cursor's own update/delete.
begin;
declare u cursor for select * from a165_edge;
fetch 2 from u;
update a165_edge set v = 'changed' where current of u;
fetch prior from u;
fetch next from u;
rollback;
begin;
declare d cursor for select * from a165_edge;
fetch 2 from d;
delete from a165_edge where current of d;
fetch prior from d;
fetch next from d;
rollback;
-- Discarding earlier rows must retain the multiple-reference error.
begin;
declare m no scroll cursor for select a.id, b.id from a165_edge a join a165_edge b on a.id = b.id for update of a, b;
fetch next from m;
fetch next from m;
update a165_edge set v = 'invalid' where current of m;
rollback;
-- A keyed NO SCROLL holdable cursor persists only its remaining rows.
begin;
declare h no scroll cursor with hold for select * from a165_edge;
fetch 2 from h;
commit;
fetch all from h;
update a165_edge set v = 'invalid' where current of h;
close h;
drop table a165_edge;
