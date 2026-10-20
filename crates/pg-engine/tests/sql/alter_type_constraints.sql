-- ALTER COLUMN TYPE makes the column's constraints anew: a check merges again with a
-- child's own same-named one; foreign keys must still go together.
create table atc_p (a float check (a > 10.2), b float);
create table atc_c () inherits (atc_p);
alter table atc_p alter column a type numeric;
alter table atc_p add constraint bnoinherit check (b > 100) no inherit;
alter table atc_c add constraint blocal check (b < 1000);
alter table atc_c add constraint bmerged check (b > 1);
alter table atc_p add constraint bmerged check (b > 1);
alter table atc_p alter column b type numeric;
select c.relname, conname, coninhcount, conislocal, connoinherit, pg_get_constraintdef(con.oid)
  from pg_constraint con join pg_class c on c.oid = con.conrelid
  where c.relname like 'atc_%' and contype = 'c' order by 1, 2;
drop table atc_p cascade;
create table atc_k (id int primary key, t text);
create table atc_r (id text references atc_k);
create table atc_r2 (id bigint references atc_k);
alter table atc_k alter column id type text;
alter table atc_k alter column id type bigint;
alter table atc_r2 alter column id type text;
alter table atc_r2 alter column id type smallint;
create table atc_s (id int primary key, parent int references atc_s);
alter table atc_s alter column parent type text;
alter table atc_s alter column id type text;
alter table atc_s alter column id type bigint;
drop table atc_r, atc_r2, atc_k, atc_s;
