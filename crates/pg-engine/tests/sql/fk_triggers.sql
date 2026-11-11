-- Referential actions run as AFTER ROW triggers (RI_ConstraintTrigger_a_/_c_): ordering with
-- user triggers, statement triggers, deferral, and how pg_trigger shows them
drop table if exists fkt_c, fkt_p, fkt_s cascade;
create table fkt_p (id int primary key);
create table fkt_c (id int primary key, pid int references fkt_p on delete cascade);
insert into fkt_p values (1),(2);
insert into fkt_c values (10,1),(11,1),(20,2);
create function fkt_tf() returns trigger language plpgsql as $$
begin
  raise notice '% % % % cnt=%', tg_name, tg_when, tg_op, coalesce(old::text, new::text), (select count(*) from fkt_c);
  return coalesce(new, old);
end $$;
create trigger aaa after delete on fkt_p for each row execute function fkt_tf();
create trigger zzz after delete on fkt_p for each row execute function fkt_tf();
create trigger "AAA" after delete on fkt_p for each row execute function fkt_tf();
create trigger bef before delete on fkt_p for each row execute function fkt_tf();
create trigger cbef before delete on fkt_c for each row execute function fkt_tf();
create trigger caft after delete on fkt_c for each row execute function fkt_tf();
delete from fkt_p;
-- self ref
create table fkt_s (id int primary key, pid int references fkt_s);
insert into fkt_s values (2,1),(1,null);
select * from fkt_s;
update fkt_s set id = id + 10, pid = pid + 10;
select * from fkt_s;
-- error timing: FK vs unique
insert into fkt_c values (30, 99), (30, 1);
insert into fkt_c values (31, 99), (32, 1);
select tgname ~ '^RI_ConstraintTrigger_[ac]_\d+$', tgrelid::regclass, tgfoid::regproc, tgtype, tgenabled, tgisinternal, tgconstrrelid::regclass, tgconstrindid::regclass, tgconstraint = (select oid from pg_constraint where conname='fkt_c_pid_fkey'), tgdeferrable, tginitdeferred, tgnargs, tgattr, tgargs, tgqual, tgoldtable, tgparentid from pg_trigger where tgconstraint in (select oid from pg_constraint where conname='fkt_c_pid_fkey') order by oid;
select t.oid::int8 - c.oid::int8, substr(tgname, 1, 22), (regexp_match(tgname,'\d+$'))[1]::oid = t.oid from pg_trigger t join pg_constraint c on c.oid = t.tgconstraint where c.conname = 'fkt_c_pid_fkey' order by t.oid;
drop table if exists fkt_c, fkt_p, fkt_g, fkt_d, fkt_s cascade;
drop function if exists fkt_tf, fkt_st cascade;
create function fkt_tf() returns trigger language plpgsql as $$
begin
  raise notice '% % % % on %', tg_name, tg_when, tg_op, coalesce(old::text, new::text), tg_table_name;
  return coalesce(new, old);
end $$;
create function fkt_st() returns trigger language plpgsql as $$
begin
  raise notice 'stmt % % % on %', tg_name, tg_when, tg_op, tg_table_name;
  return null;
end $$;
-- chain with statement triggers
create table fkt_p (id int primary key);
create table fkt_c (id int primary key, pid int references fkt_p on delete cascade on update cascade);
create table fkt_g (id int primary key, cid int references fkt_c on delete cascade on update set null);
insert into fkt_p values (1),(2);
insert into fkt_c values (10,1),(11,1),(20,2);
insert into fkt_g values (100,10),(110,11),(200,20);
create trigger ca after delete or update on fkt_c for each row execute function fkt_tf();
create trigger ga after delete or update on fkt_g for each row execute function fkt_tf();
create trigger pa after delete or update on fkt_p for each row execute function fkt_tf();
create trigger s1 before delete or update on fkt_c for each statement execute function fkt_st();
create trigger s2 after delete or update on fkt_c for each statement execute function fkt_st();
create trigger s3 after delete or update on fkt_p for each statement execute function fkt_st();
create trigger s4 after delete or update on fkt_g for each statement execute function fkt_st();
update fkt_p set id = id * 5;
select * from fkt_c order by id;
select * from fkt_g order by id;
delete from fkt_p;
select count(*) from fkt_c;
select count(*) from fkt_g;
-- transition tables across cascades
drop trigger s2 on fkt_c;
create function fkt_tt() returns trigger language plpgsql as $$
begin
  raise notice 'tt % rows=%', tg_name, (select count(*) from oldt);
  return null;
end $$;
create trigger s5 after delete on fkt_c referencing old table as oldt for each statement execute function fkt_tt();
insert into fkt_p values (1),(2);
insert into fkt_c values (10,1),(11,1),(20,2);
delete from fkt_p;
-- NO ACTION key swap ok, RESTRICT not
drop table fkt_g, fkt_c, fkt_p cascade;
create table fkt_p (id int primary key);
create table fkt_c (pid int references fkt_p);
create table fkt_d (pid int references fkt_p on update restrict);
insert into fkt_p values (1),(2);
insert into fkt_c values (1),(2);
update fkt_p set id = 3 - id;
insert into fkt_d values (1);
update fkt_p set id = 3 - id;
-- violation vs other error ordering
insert into fkt_c values (5);
insert into fkt_c values (5), (null), (1/0);
insert into fkt_c select x from generate_series(1,3) x;
-- deferred
create table fkt_s (id int primary key, pid int references fkt_s deferrable initially deferred);
begin;
insert into fkt_s values (1, 2);
insert into fkt_s values (3, 4);
update fkt_s set pid = 1 where id = 3;
select 'mid';
commit;
begin;
insert into fkt_s values (1, 2);
update fkt_s set pid = 5 where id = 1;
set constraints all immediate;
rollback;
begin;
insert into fkt_s values (1, 1), (2, 1);
set constraints fkt_s_pid_fkey immediate;
delete from fkt_s where id = 1;
commit;
select * from fkt_s;
drop table if exists fkt_c, fkt_p, fkt_pp, fkt_cp, fkt_m cascade;
drop function if exists fkt_tf, fkt_f cascade;
create table fkt_p (id int primary key, v int);
create table fkt_c (id int, pid int references fkt_p);
insert into fkt_p values (1, 0), (2, 0);
copy fkt_c from stdin;
1	1
2	9
3	2
\.
-- error inside function: context
create function fkt_f(x int) returns void language plpgsql as $$
begin
  insert into fkt_c values (x, x);
end $$;
select fkt_f(1);
select fkt_f(7);
-- ON CONFLICT
create unique index on fkt_c (id);
insert into fkt_c values (1, 2) on conflict (id) do update set pid = 8;
insert into fkt_c values (1, 2) on conflict (id) do update set pid = excluded.pid;
-- writable CTE
with x as (delete from fkt_p where id = 2 returning id) insert into fkt_c select id + 10, id from x;
with x as (insert into fkt_p values (3, 0) returning id) insert into fkt_c select 30, id from x;
-- MERGE
merge into fkt_c c using (values (40, 4)) s(id, pid) on c.id = s.id when not matched then insert values (s.id, s.pid);
merge into fkt_c c using (values (30, 1)) s(id, pid) on c.id = s.id when matched then update set pid = 77;
-- replica role skips RI
set session_replication_role = replica;
insert into fkt_c values (50, 555);
reset session_replication_role;
select * from fkt_c order by id;
delete from fkt_c where id = 50;
-- disable trigger all
alter table fkt_c disable trigger all;
insert into fkt_c values (51, 556);
select tgname ~ '^RI_', tgenabled from pg_trigger where tgrelid = 'fkt_c'::regclass order by 1, 2;
alter table fkt_c enable trigger all;
delete from fkt_c where id = 51;
alter table fkt_c disable trigger user;
select tgname ~ '^RI_', tgenabled from pg_trigger where tgrelid = 'fkt_c'::regclass order by 1, 2;
alter table fkt_c enable trigger all;
-- truncate
truncate fkt_p;
-- crosspart move of referenced partitioned table
create table fkt_pp (id int primary key) partition by range (id);
create table fkt_pp1 partition of fkt_pp for values from (0) to (100);
create table fkt_pp2 partition of fkt_pp for values from (100) to (200);
create table fkt_cp (pid int references fkt_pp on update cascade on delete cascade);
insert into fkt_pp values (1), (2);
insert into fkt_cp values (1), (2);
update fkt_pp set id = 150 where id = 1;
select * from fkt_cp order by 1;
delete from fkt_pp where id = 150;
select * from fkt_cp order by 1;
create table fkt_m (pid int references fkt_pp);
insert into fkt_m values (2);
update fkt_pp set id = 160 where id = 2;
update fkt_pp set id = 3 where id = 2;
select tgrelid::regclass, tgfoid::regproc, tgparentid <> 0, tgconstrrelid::regclass, tgconstrindid::regclass, tgdeferrable from pg_trigger where tgisinternal and tgconstraint in (select oid from pg_constraint where conrelid = 'fkt_cp'::regclass) order by tgrelid::regclass::text, tgfoid::regproc::text;
drop table if exists fkt_c, fkt_p cascade;
create table fkt_p (id int primary key);
create table fkt_c (id int, pid int references fkt_p on delete set null deferrable initially deferred);
select regexp_replace(pg_get_triggerdef(oid), '\d+', 'N', 'g') from pg_trigger where tgrelid in ('fkt_p'::regclass, 'fkt_c'::regclass) order by tgrelid::regclass::text, tgfoid;
select regexp_replace(pg_describe_object('pg_trigger'::regclass, oid, 0), '\d+', 'N', 'g') from pg_trigger where tgrelid in ('fkt_p'::regclass, 'fkt_c'::regclass) order by 1;
select (pg_identify_object('pg_trigger'::regclass, oid, 0)).type, regexp_replace((pg_identify_object('pg_trigger'::regclass, oid, 0)).identity, '\d+', 'N', 'g') from pg_trigger where tgrelid in ('fkt_p'::regclass, 'fkt_c'::regclass) order by 2;
select classid::regclass, refclassid::regclass, deptype, count(*) from pg_depend where classid = 'pg_trigger'::regclass and objid in (select oid from pg_trigger where tgrelid in ('fkt_p'::regclass, 'fkt_c'::regclass)) group by 1,2,3 order by 1,2,3;
select relname, relhastriggers from pg_class where relname like 'fkt_%' and relkind = 'r' order by 1;
select tablename, hastriggers from pg_tables where tablename like 'fkt_%' order by 1;
alter table fkt_c disable trigger all;
alter table fkt_c enable trigger all;
alter table fkt_c alter constraint fkt_c_pid_fkey not deferrable;
select tgfoid::regproc, tgdeferrable, tginitdeferred from pg_trigger where tgrelid in ('fkt_p'::regclass, 'fkt_c'::regclass) order by tgrelid::regclass::text, tgfoid;
alter table fkt_c drop constraint fkt_c_pid_fkey;
select count(*) from pg_trigger where tgrelid in ('fkt_p'::regclass, 'fkt_c'::regclass);
-- relhastriggers stays set after the triggers go, until VACUUM
create table fkt_u (a int);
create function fkt_noop() returns trigger language plpgsql as $$ begin return null; end $$;
create trigger u1 after insert on fkt_u for each statement execute function fkt_noop();
drop trigger u1 on fkt_u;
select relname, relhastriggers from pg_class where relname in ('fkt_u', 'fkt_d', 'fkt_c', 'fkt_p') order by 1;
vacuum fkt_u, fkt_d, fkt_c, fkt_p;
select relname, relhastriggers from pg_class where relname in ('fkt_u', 'fkt_d', 'fkt_c', 'fkt_p') order by 1;
drop table fkt_u;
drop function fkt_noop;
drop table if exists fkt_c, fkt_p, fkt_g, fkt_d, fkt_s, fkt_pp, fkt_cp, fkt_m cascade;
drop function if exists fkt_tf, fkt_st, fkt_tt, fkt_f cascade;
