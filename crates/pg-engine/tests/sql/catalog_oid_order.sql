-- oid allocation order: relation, array type, rowtype, attrdefs, constraints, toast
create table t (a int default 1, b text default 'x', c int generated always as (a*2) stored, d int generated always as (a*3) virtual);
select c.reltype::int8 - c.oid::int8 as rowtype, t.typarray::int8 - c.oid::int8 as arr, c.reltoastrelid::int8 - c.oid::int8 as toast from pg_class c join pg_type t on t.oid = c.reltype where c.relname = 't';
select adnum, oid::int8 - 't'::regclass::oid::int8 from pg_attrdef where adrelid = 't'::regclass order by adnum;
create view v as select 1 as a;
select c.reltype::int8 - c.oid::int8, t.typarray::int8 - c.oid::int8, (select r.oid::int8 from pg_rewrite r where r.ev_class = c.oid) - c.oid::int8 from pg_class c join pg_type t on t.oid = c.reltype where c.relname = 'v';
create type comp as (a int, b text);
select c.reltype::int8 - c.oid::int8, t.typarray::int8 - c.oid::int8 from pg_class c join pg_type t on t.oid = c.reltype where c.relname = 'comp';
create materialized view mv as select 1 as a, 'x'::text as b;
select relname ~ '^pg_toast_\d+(_index)?$' as toast, relkind, o::int8 - 'mv'::regclass::oid::int8 from (select relname, relkind, oid o from pg_class union all select typname, 'T', oid from pg_type union all select rulename, 'R', oid from pg_rewrite) s where o >= 'mv'::regclass order by 3;
-- inheritance: inherited defaults, then local defaults, inherited checks by name, local checks, not-nulls
create table p (a int default 1 not null, b text default 'z' check (b <> ''), check (a > 0));
create table ch (c int default 3 check (c > 1), d int not null) inherits (p);
select k, x, o::int8 - 'ch'::regclass::oid::int8 from (
select 'def' k, adnum::text x, oid o from pg_attrdef where adrelid = 'ch'::regclass
union all select 'con', conname::text, oid from pg_constraint where conrelid = 'ch'::regclass
union all select 'cls', case when relname ~ '^pg_toast' then 'toast' else relname::text end, oid from pg_class where oid > 'ch'::regclass) s order by 3;
create table k (a int primary key, b text unique, c int references k, d int check (d > 0) default 4);
select k, x, o::int8 - 'k'::regclass::oid::int8 from (
select 'def' k, adnum::text x, oid o from pg_attrdef where adrelid = 'k'::regclass
union all select 'con', conname::text, oid from pg_constraint where conrelid = 'k'::regclass
union all select 'cls', case when relname ~ '^pg_toast' then 'toast' else relname::text end, oid from pg_class where oid > 'k'::regclass) s order by 3;
-- partitions
create table pt (a int default 4, b text) partition by list (a);
create table pt1 partition of pt for values in (1);
select adrelid::regclass, adnum, oid::int8 - 'pt'::regclass::oid::int8 from pg_attrdef where adrelid in ('pt'::regclass, 'pt1'::regclass) order by 1, 2;
create table pp (a int primary key, b text, c int check (c > 0)) partition by range (a);
create table pp1 partition of pp (unique (c)) for values from (1) to (10);
select case when n ~ '^pg_toast' then 'toast' else n end, o::int8 - 'pp1'::regclass::oid::int8 from (
select relname::text n, oid o from pg_class union all select conname::text, oid from pg_constraint union all select 'def ' || adrelid::regclass::text, oid from pg_attrdef union all select typname::text, oid from pg_type) s where o >= 'pp1'::regclass order by 2;
create table lk (like t including defaults);
select adnum, oid::int8 - 'lk'::regclass::oid::int8 from pg_attrdef where adrelid = 'lk'::regclass order by 1;
-- LIKE copies defaults, checks (by name) and indexes after the TOAST table and own indexes
create table s (a int default 1 not null, b text default 'x' check (b <> 'q'), c int generated always as (a*2) stored, d int, constraint zc check (d > 0), constraint ac check (d < 100), unique (d));
create index s_ix on s (a);
create table l1 (z int default 9 check (z > 0), like s including all, y text default 'y');
select case when n ~ '^pg_toast' then 'toast' else n end, o::int8 - 'l1'::regclass::oid::int8 from (select relname::text n, oid o from pg_class union all select conname::text, oid from pg_constraint union all select 'def ' || adrelid::regclass::text || adnum, oid from pg_attrdef union all select typname::text, oid from pg_type) s where o >= 'l1'::regclass order by 2;
create table l2 (like s including defaults including constraints);
select case when n ~ '^pg_toast' then 'toast' else n end, o::int8 - 'l2'::regclass::oid::int8 from (select relname::text n, oid o from pg_class union all select conname::text, oid from pg_constraint union all select 'def ' || adrelid::regclass::text || adnum, oid from pg_attrdef union all select typname::text, oid from pg_type) s where o >= 'l2'::regclass order by 2;
create table l3 (q int, like s including generated);
select case when n ~ '^pg_toast' then 'toast' else n end, o::int8 - 'l3'::regclass::oid::int8 from (select relname::text n, oid o from pg_class union all select conname::text, oid from pg_constraint union all select 'def ' || adrelid::regclass::text || adnum, oid from pg_attrdef union all select typname::text, oid from pg_type) s where o >= 'l3'::regclass order by 2;
create table s2 (a int default 1, b text default 'x' check (b <> 'q'), constraint zc check (a > 0), constraint ac check (a < 100));
create table s3 (e int default 5 check (e > 0), f text);
create table l4 (k int primary key, like s2 including all, u int references l4, like s3 including all, r int unique);
select case when n ~ '^pg_toast' then 'toast' else n end, o::int8 - 'l4'::regclass::oid::int8 from (select relname::text n, oid o from pg_class union all select conname::text, oid from pg_constraint union all select 'def ' || adrelid::regclass::text || adnum, oid from pg_attrdef union all select typname::text, oid from pg_type) s where o >= 'l4'::regclass order by 2;
create table ct as select 1 as a, 'x'::text as b;
select case when n ~ '^pg_toast' then 'toast' else n end, o::int8 - 'ct'::regclass::oid::int8 from (
select relname::text n, oid o from pg_class union all select typname::text, oid from pg_type) s where o >= 'ct'::regclass order by 2;
-- ALTER TABLE defaults get fresh attrdef oids
create table q (a int default 1, b int default 2);
create table mark1 ();
alter table q alter column a set default 10;
alter table q alter column b drop default;
alter table q add column c int default 7;
alter table q alter column b set default 3;
create table mark2 ();
select adnum, oid::int8 - 'mark1'::regclass::oid::int8 from pg_attrdef where adrelid = 'q'::regclass order by 1;
select 'mark2'::regclass::oid::int8 - 'mark1'::regclass::oid::int8;
alter table q alter column a type bigint;
create table mark3 ();
select adnum, oid::int8 - 'mark2'::regclass::oid::int8 from pg_attrdef where adrelid = 'q'::regclass order by 1;
select 'mark3'::regclass::oid::int8 - 'mark2'::regclass::oid::int8;
create table al (a int);
alter table al add column b text default 'x', add primary key (a);
create table mk ();
select case when n ~ '^pg_toast' then 'toast' else n end, o::int8 - 'al'::regclass::oid::int8 from (
select relname::text n, oid o from pg_class union all select conname::text, oid from pg_constraint union all select 'def ' || adrelid::regclass::text, oid from pg_attrdef union all select typname::text, oid from pg_type) s where o >= 'al'::regclass order by 2;
-- toast tables only for tables with wide or variable-length columns
create table nt (a int, b float8, c varchar(100));
create table wt (a int, b varchar(1000));
select relname, reltoastrelid <> 0 from pg_class where relname in ('nt', 'wt', 't', 'q') order by 1;
-- toast catalog rows
create table tt (a int, b text);
select c.relname ~ '^pg_toast_\d+(_index)?$', relnamespace::regnamespace, reltype, reloftype, relowner::regrole, relam, relfilenode = c.oid, reltablespace, relpages, reltuples, relallvisible, relallfrozen, reltoastrelid, relhasindex, relisshared, relpersistence, relkind, relnatts, relchecks, relhasrules, relhastriggers, relhassubclass, relrowsecurity, relforcerowsecurity, relispopulated, relreplident, relispartition, relrewrite, relfrozenxid::text::int8 > 0, relminmxid, relacl, reloptions, relpartbound from pg_class c where oid in (select reltoastrelid from pg_class where relname = 'tt') or oid in (select indexrelid from pg_index where indrelid = (select reltoastrelid from pg_class where relname = 'tt')) order by relkind desc;
select relname = 'pg_toast_' || 'tt'::regclass::oid from pg_class where oid = (select reltoastrelid from pg_class where relname = 'tt');
select attname, atttypid::regtype, attlen, attnum, atttypmod, attndims, attbyval, attalign, attstorage, attcompression, attnotnull, atthasdef, attidentity, attgenerated, attisdropped, attislocal, attinhcount, attcollation, attstattarget, attacl, attoptions from pg_attribute where attrelid = (select reltoastrelid from pg_class where relname = 'tt') order by attnum;
select attname, atttypid::regtype, attlen, attnum, attstorage, attnotnull, attislocal, attcollation from pg_attribute where attrelid = (select indexrelid from pg_index where indrelid = (select reltoastrelid from pg_class where relname = 'tt')) order by attnum;
select indnatts, indnkeyatts, indisunique, indnullsnotdistinct, indisprimary, indisexclusion, indimmediate, indisclustered, indisvalid, indcheckxmin, indisready, indislive, indisreplident, indkey, indcollation, indclass, indoption, indexprs, indpred from pg_index where indrelid = (select reltoastrelid from pg_class where relname = 'tt');
select classid::regclass, case when objid = (select reltoastrelid from pg_class where relname = 'tt') then 'toast' when objid = 'tt'::regclass then 'tbl' else 'idx' end, objsubid, refclassid::regclass, case when refobjid = (select reltoastrelid from pg_class where relname = 'tt') then 'toast' when refobjid = 'tt'::regclass then 'tbl' else refobjid::text end, refobjsubid, deptype from pg_depend where objid in (select reltoastrelid from pg_class where relname = 'tt') or objid in (select indexrelid from pg_index where indrelid = (select reltoastrelid from pg_class where relname = 'tt')) or refobjid in (select reltoastrelid from pg_class where relname = 'tt') order by 1, 2, 3, 5;
select count(*) from pg_type where typrelid = (select reltoastrelid from pg_class where relname = 'tt');
select pg_describe_object('pg_class'::regclass, reltoastrelid, 0) ~ '^toast table pg_toast\.pg_toast_\d+$', pg_describe_object('pg_class'::regclass, reltoastrelid, 1) ~ '^column chunk_id of toast table pg_toast\.pg_toast_\d+$', pg_describe_object('pg_class'::regclass, (reltoastrelid::int8 + 1)::oid, 0) ~ '^index pg_toast\.pg_toast_\d+_index$' from pg_class where relname = 'tt';
select (pg_identify_object('pg_class'::regclass, reltoastrelid, 0)).type, (pg_identify_object('pg_class'::regclass, reltoastrelid, 1)).type, (pg_identify_object('pg_class'::regclass, (reltoastrelid::int8 + 1)::oid, 0)).type from pg_class where relname = 'tt';
select reltoastrelid::regclass::text ~ '^pg_toast\.pg_toast_\d+$', ('pg_toast.pg_toast_' || 'tt'::regclass::oid)::regclass = reltoastrelid from pg_class where relname = 'tt';
select schemaname, relname ~ '^pg_toast', seq_scan from pg_stat_all_tables where relid = (select reltoastrelid from pg_class where relname = 'tt');
select schemaname, relname ~ '^pg_toast', indexrelname ~ 'index' from pg_stat_all_indexes where relid = (select reltoastrelid from pg_class where relname = 'tt');
select count(*) from pg_stat_sys_tables where relid = (select reltoastrelid from pg_class where relname = 'tt');
select count(*) from pg_tables where schemaname = 'pg_toast';
select count(*) from information_schema.tables where table_schema = 'pg_toast';
-- temp and unlogged toast
create unlogged table ut (a text);
select relpersistence, relkind from pg_class where oid = (select reltoastrelid from pg_class where relname = 'ut') or oid = (select indexrelid from pg_index where indrelid = (select reltoastrelid from pg_class where relname = 'ut')) order by 2 desc;
create temp table tmp (a text);
select relnamespace::regnamespace::text ~ '^pg_toast_temp_\d+$', relpersistence, relkind from pg_class where oid = (select reltoastrelid from pg_class where relname = 'tmp') or oid = (select indexrelid from pg_index where indrelid = (select reltoastrelid from pg_class where relname = 'tmp')) order by 3 desc;
select t.oid::int8 - n.oid::int8 from pg_namespace n, pg_namespace t where n.nspname ~ '^pg_temp_\d+$' and t.nspname ~ '^pg_toast_temp_\d+$';
-- frozen xids and system columns per relkind
create table x (a int); create table xp (a int) partition by list (a); create sequence xs; create view xv as select 1; create materialized view xm as select 1; create index on x (a);
select relkind, relfrozenxid::text::int8 > 0, relminmxid from pg_class where relname in ('x', 'xp', 'xs', 'xv', 'xm', 'x_a_idx', 'comp') order by 1;
select attrelid::regclass, count(*) from pg_attribute where attnum < 0 and attrelid in ('x'::regclass, 'xp'::regclass, 'xs'::regclass, 'xv'::regclass, 'xm'::regclass, 'x_a_idx'::regclass, 'comp'::regclass) group by 1 order by 1;
-- rewrites consume oids for the new heap, toast and index files
create table m0 ();
create table ra (a int primary key, b text, c varchar(10), d int);
create index ra_d on ra (d);
create table m1 ();
alter table ra alter column c type varchar(20);
create table m2 ();
alter table ra alter column d type bigint;
create table m3 ();
vacuum full ra;
create table m4 ();
cluster ra using ra_pkey;
create table m5 ();
truncate ra;
create table m6 ();
alter table ra add column e float8 default random();
create table m7 ();
alter table ra set unlogged;
create table m8 ();
create table rb (a int);
create table m9 ();
truncate rb;
create table m10 ();
vacuum full rb;
create table m11 ();
create materialized view rmv as select 1 as a, 'x'::text as b;
create index on rmv (a);
create table m12 ();
refresh materialized view rmv;
create table m13 ();
alter table rb alter column a type text;
create table m14 ();
select relname, oid::int8 - lag(oid::int8) over (order by oid) - 3 from pg_class where relname ~ '^m\d+$' order by oid;
select relname, reltoastrelid <> 0 from pg_class where relname in ('ra', 'rb', 'rmv') order by 1;
create table n0 ();
create table z (a int primary key, b int unique, c int check (c > 0), d int);
create index z_d on z (d);
create table n1 ();
alter table z alter column a type bigint;
create table n2 ();
alter table z alter column c type bigint;
create table n3 ();
alter table z alter column d type int8, alter column b type int8;
create table n4 ();
alter table z alter column d type int4 using d::int4;
create table n5 ();
alter table z add column e int default 5;
create table n6 ();
alter table z add column g text generated always as ('x') stored;
create table n7 ();
alter table z add column h int generated always as identity;
create table n8 ();
create table y (a text, b numeric(10,2), c timestamp(3), e cidr, f varchar);
create table n9 ();
alter table y alter column a type varchar;
create table n10 ();
alter table y alter column b type numeric(12,2), alter column c type timestamp(6), alter column e type inet;
create table n11 ();
alter table y alter column f type varchar(5);
create table n12 ();
select relname, oid::int8 - lag(oid::int8) over (order by oid) - 3 from pg_class where relname ~ '^n\d+$' order by oid;
