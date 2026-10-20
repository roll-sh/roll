-- Transition tables under referential actions: the cascaded rows join the statement's
-- transition data until a trigger reads it; then a new one starts (and BEFORE STATEMENT
-- triggers fire again).
create function tc_bs() returns trigger language plpgsql as $$
begin
  raise notice 'before statement on %', tg_table_name;
  return null;
end $$;
create function tc_dump() returns trigger language plpgsql as $$
begin
  raise notice 'trigger = %, old table = %', tg_name,
    (select string_agg(old_table::text, ', ' order by a) from old_table);
  return null;
end $$;
create table tc_self (a int primary key, b int references tc_self(a) on delete cascade);
create trigger tc_bs before delete on tc_self for each statement execute function tc_bs();
create trigger tc_r after delete on tc_self referencing old table as old_table
  for each row execute function tc_dump();
create trigger tc_s after delete on tc_self referencing old table as old_table
  for each statement execute function tc_dump();
insert into tc_self values (1, null), (2, 1), (3, 2);
delete from tc_self where a = 1;
drop trigger tc_r on tc_self;
insert into tc_self values (1, null), (2, 1), (3, 2), (4, 3);
delete from tc_self where a = 1;
-- a cascade to another table
create table tc_p (a int primary key);
create table tc_c (a int, p int references tc_p on delete cascade);
create trigger tc_bs before delete on tc_c for each statement execute function tc_bs();
create trigger tc_s after delete on tc_c referencing old table as old_table
  for each statement execute function tc_dump();
create trigger tc_s after delete on tc_p referencing old table as old_table
  for each statement execute function tc_dump();
insert into tc_p values (1), (2);
insert into tc_c values (10, 1), (11, 1), (20, 2);
delete from tc_p;
drop table tc_self, tc_c, tc_p;
drop function tc_bs(), tc_dump();
