-- ALTER COLUMN TYPE re-creates the indexes and constraints on the column with new oids:
-- index-backed constraints (index, then constraint), plain indexes, then the other
-- constraints (not-null first; foreign keys with their triggers), then other tables' keys
create table r (a int primary key);
create table t (id int, a int not null, b text, c int, constraint ck1 check (a > 0), constraint ck2 check (c > 0), unique (b), constraint t_pk primary key (a, id), constraint fk1 foreign key (a) references r, constraint ck0 check (a < 1000));
create index ix2 on t (b, a);
create index ix1 on t ((a + 1));
create index ix0 on t (c) where a > 5;
create table t2 (x int, y int, foreign key (x, y) references t (a, id));
comment on index ix1 is 'ix1 comment';
comment on constraint ck1 on t is 'ck1 comment';
create table mk ();
alter table t alter column a type bigint;
create table mk2 ();
select case when n ~ '^pg_toast' then 'toast' else regexp_replace(n, '_\d+$', '') end, o::int8 - 'mk'::regclass::oid::int8 from (
select relname::text n, oid o from pg_class union all select 'con ' || conname::text, oid from pg_constraint union all select 'def ' || adrelid::regclass::text, oid from pg_attrdef union all select 'type ' || typname::text, oid from pg_type
union all select 'trig ' || tgname, oid from pg_trigger) s where o >= 'mk'::regclass order by 2;
select obj_description('ix1'::regclass, 'pg_class'), obj_description((select oid from pg_constraint where conname = 'ck1'), 'pg_constraint');
select conname, conindid::regclass, contype from pg_constraint where conrelid in ('t'::regclass, 't2'::regclass) order by 1;
select indexrelid::regclass, indisprimary, indisunique from pg_index where indrelid = 't'::regclass order by 1;
select count(*) from pg_trigger where tgconstraint in (select oid from pg_constraint where conrelid in ('t'::regclass, 't2'::regclass));
select classid::regclass, refclassid::regclass, deptype, count(*) from pg_depend where refobjid = 't'::regclass and refobjsubid = 2 group by 1, 2, 3 order by 1, 2, 3;
insert into r values (1);
insert into t values (1, 1, 'x', 1);
insert into t values (1, 1, 'y', 1);
insert into t values (1, 2, 'z', 1);
drop table t2, t, r, mk, mk2;
-- several columns in one statement, an inheritance child, statistics, a later ADD CONSTRAINT
create table t (id int not null, a int not null, b int, constraint ckab check (a > b), constraint cka check (a > 0), constraint ckid check (id > 0), unique (id), unique (a));
create index ixa on t (a);
create index ixid on t (id);
create index ixab on t (a, id);
create statistics st on a, b from t;
create table c (z int) inherits (t);
create index ixc on c (a);
create table mk1 ();
alter table t alter column a type bigint, alter column id type bigint, add constraint ckz check (b > 1);
create table mk2 ();
select case when n ~ '^pg_toast' then 'toast' else n end, o::int8 - 'mk1'::regclass::oid::int8 from (
select relname::text n, oid o from pg_class union all select 'con ' || conname::text || ' ' || conrelid::regclass, oid from pg_constraint union all select 'def ' || adrelid::regclass::text, oid from pg_attrdef union all select 'type ' || typname::text, oid from pg_type
union all select 'stat ' || stxname, oid from pg_statistic_ext) s where o >= 'mk1'::regclass order by 2;
drop table c, t, mk1, mk2;
-- exclusion, deferrable unique (with its trigger), NOT ENFORCED key, partial index, ADD COLUMN
create table t (id int, a int, b int, constraint ex exclude using btree (a with =), constraint fk foreign key (b) references t (id) not enforced, primary key (id), unique (b) deferrable);
create index ix on t (a) where b > 0;
create table mk1 ();
alter table t alter column a type bigint, alter column id type bigint, add column z int default 3, alter column b type bigint;
create table mk2 ();
select case when n ~ '^pg_toast' then 'toast' else regexp_replace(n, '_\d+$', '') end, o::int8 - 'mk1'::regclass::oid::int8 from (
select relname::text n, oid o from pg_class union all select 'con ' || conname::text || ' ' || conrelid::regclass, oid from pg_constraint union all select 'def ' || adrelid::regclass::text, oid from pg_attrdef union all select 'type ' || typname::text, oid from pg_type
union all select 'trig ' || tgname, oid from pg_trigger) s where o >= 'mk1'::regclass order by 2;
select conname, conindid::regclass, contype, condeferrable from pg_constraint where conrelid = 't'::regclass order by 1;
drop table t, mk1, mk2;
-- inherited check constraints come back with the parent's, depth first
create table p (a int, b int, constraint pc check (a > b));
create table c1 () inherits (p);
create table c2 () inherits (p);
create table c3 () inherits (c1);
create table mk1 ();
alter table p alter column a type bigint;
select conrelid::regclass, conname, conislocal, coninhcount, oid::int8 - 'mk1'::regclass::oid::int8 from pg_constraint where conname = 'pc' order by 5;
drop table p cascade;
drop table mk1;
-- typed tables follow ALTER TYPE ... CASCADE
create type ct as (a int, b text);
create table tt of ct (primary key (a), check (a > 0));
create index on tt (b, a);
create table mk1 ();
alter type ct alter attribute a type bigint cascade;
create table mk2 ();
select case when n ~ '^pg_toast' then 'toast' else n end, o::int8 - 'mk1'::regclass::oid::int8 from (
select relname::text n, oid o from pg_class union all select 'con ' || conname::text || ' ' || conrelid::regclass, oid from pg_constraint) s where o >= 'mk1'::regclass order by 2;
drop table tt, mk1, mk2;
drop type ct;
-- deferrable primary key, unique and exclusion constraints have an internal trigger
create table mk ();
create table u (a int primary key deferrable initially deferred, b int unique deferrable, c int, exclude using btree (c with =) deferrable);
select tgname ~ ('_' || oid || '$'), regexp_replace(tgname, '_\d+$', ''), tgrelid::regclass, tgparentid, tgfoid::regproc, tgtype, tgenabled, tgisinternal, tgconstrrelid, tgconstrindid::regclass, (select conname from pg_constraint where oid = tgconstraint), tgdeferrable, tginitdeferred, tgnargs, tgattr, tgargs, tgqual, oid::int8 - 'mk'::regclass::int8 from pg_trigger where tgrelid = 'u'::regclass order by oid;
select oid::int8 - 'mk'::regclass::int8, contype from pg_constraint where conrelid = 'u'::regclass order by 1;
select classid::regclass, objsubid, refclassid::regclass, refobjsubid, deptype from pg_depend where classid = 'pg_trigger'::regclass and objid in (select oid from pg_trigger where tgrelid = 'u'::regclass);
select relhastriggers from pg_class where oid = 'u'::regclass;
select regexp_replace(pg_get_triggerdef(oid), '_\d+"', '"') from pg_trigger where tgrelid = 'u'::regclass order by oid;
select regexp_replace(pg_describe_object(classid, objid, objsubid), '_\d+ ', ' ') from pg_depend where classid = 'pg_trigger'::regclass and objid in (select oid from pg_trigger where tgrelid = 'u'::regclass) order by objid;
create table u2 (like u including all);
select regexp_replace(tgname, '_\d+$', ''), (select conname from pg_constraint where oid = tgconstraint) from pg_trigger where tgrelid = 'u2'::regclass order by oid;
alter table u disable trigger all;
select tgenabled from pg_trigger where tgrelid = 'u'::regclass;
alter table u enable trigger all;
alter table u drop constraint u_b_key;
select count(*) from pg_trigger where tgrelid = 'u'::regclass;
begin;
insert into u values (1, 1, 1), (1, 2, 2);
update u set a = 2 where b = 2;
commit;
create table pp (a int, primary key (a) deferrable) partition by range (a);
create table pp1 partition of pp for values from (1) to (5);
select tgrelid::regclass, regexp_replace(tgname, '_\d+$', ''), tgparentid <> 0 from pg_trigger where tgrelid in ('pp'::regclass, 'pp1'::regclass) order by oid;
drop table u, u2, mk, pp;
