-- ALTER COLUMN TYPE on a partitioned table re-creates the indexes and constraints on the
-- column with new oids: the partitioned ones recursing to the partitions (whose indexes are
-- named anew), the partitions' own, then not-null and check constraints, then foreign keys
create table p (k int, a int, b int, constraint ck check (a > 0), primary key (k, a)) partition by range (k);
create index p_b on p (b, a);
create index p_k on p (k);
create unique index p_a_u on p (a, k);
comment on index p_b is 'b index';
create table p1 partition of p for values from (0) to (10);
create table p2 (b int, a int not null, k int not null, constraint ck check (a > 0));
create index p2_own on p2 (a);
create index p2_b on p2 (b, a);
alter table p attach partition p2 for values from (10) to (20);
create table p3 partition of p for values from (20) to (30) partition by range (k);
create table p3a partition of p3 for values from (20) to (25);
create table r (x int, z int, foreign key (x, z) references p);
create table mk0 ();
create view objs as select 'c' k, conname::text n, conrelid::regclass::text r, oid::int8 - (select oid::int8 from pg_class where relname = 'mk0') d from pg_constraint where conrelid in (select oid from pg_class where relnamespace = 'public'::regnamespace) union all select 'i', relname, '', oid::int8 - (select oid::int8 from pg_class where relname = 'mk0') from pg_class where relkind in ('i','I') and relnamespace = 'public'::regnamespace;
create table mk1 ();
alter table p alter a type bigint;
create table mk2 ();
select k, n, r, d - (select oid::int8 - (select oid::int8 from pg_class where relname = 'mk0') from pg_class where relname = 'mk1') from objs where d > 0 order by d;
select k, n, r from objs where d < 0 order by n, r;
select (select oid from pg_class where relname = 'mk2')::int8 - (select oid from pg_class where relname = 'mk1')::int8;
select conname, conrelid::regclass, conparentid <> 0, conindid::regclass, conislocal, coninhcount from pg_constraint where conrelid::regclass::text ~ '^(p|r)' and contype in ('p','u','f','n','c') order by conrelid::regclass::text, conname;
select indexrelid::regclass::text, inhparent::regclass::text from pg_index left join pg_inherits on inhrelid = indexrelid where indrelid::regclass::text ~ '^p' order by 1, 2;
select tgrelid::regclass, tgfoid::regproc, oid::int8 - (select oid::int8 from pg_class where relname = 'mk1') from pg_trigger where tgrelid::regclass::text ~ '^(p|r)' order by oid;
select obj_description('p_b'::regclass, 'pg_class');
insert into p values (1, 2, 3), (11, 4, 5), (21, 6, 1);
insert into r values (1, 2);
insert into r values (1, 3);
delete from p where k = 1;
select * from p order by k;
drop view objs;
drop table r, p, mk0, mk1, mk2;
-- a foreign key from a partitioned table: the partitions' copies come back after it
create table pk (a int primary key);
create table f (k int, a int references pk) partition by range (k);
create table f1 partition of f for values from (0) to (10);
create table f2 partition of f for values from (10) to (20);
create table mk0 ();
alter table pk alter a type bigint;
create table mk1 ();
select conname, conrelid::regclass, oid::int8 - (select oid::int8 from pg_class where relname = 'mk0') from pg_constraint where contype = 'f' order by oid;
select tgrelid::regclass, tgfoid::regproc, oid::int8 - (select oid::int8 from pg_class where relname = 'mk0') from pg_trigger where tgrelid::regclass::text in ('pk', 'f', 'f1', 'f2') order by oid;
select (select oid from pg_class where relname = 'mk1')::int8 - (select oid from pg_class where relname = 'mk0')::int8;
drop table f, pk, mk0, mk1;
-- inheritance: a child's merged not-null constraint comes back under the parent's name
create table ip (a int not null, b int, constraint ick check (a > 0));
create table ic (a int, constraint ic_nn not null a, constraint ick check (a > 0)) inherits (ip);
create table mk0 ();
alter table ip alter a type bigint;
create table mk1 ();
select conname, conrelid::regclass, conislocal, coninhcount, oid::int8 - (select oid::int8 from pg_class where relname = 'mk0') from pg_constraint where conrelid in ('ip'::regclass, 'ic'::regclass) order by oid;
drop table ip, ic, mk0, mk1;
