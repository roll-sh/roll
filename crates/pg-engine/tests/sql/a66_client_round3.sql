-- Client suites, round 3 (agent 66): DML on pg_description and pg_class, one-field
-- composites unnested in FROM, INSERT ... SELECT parameter typing, btree_gist's text class
-- for varchar, extension types in information_schema, GUC_IS_NAME truncation.

-- pgjdbc's DatabaseMetaDataTest: a function's comment moved onto a table (its classoid kept).
create table duplicate (x text);
comment on table duplicate is 'duplicate table';
create function bar() returns integer language sql as $$ select 1 $$;
comment on function bar() is 'bar function';
update pg_description set objoid = 'duplicate'::regclass where objoid = 'bar'::regproc;
select classoid::regclass, objsubid, description from pg_description
 where objoid = 'duplicate'::regclass order by 3;
select obj_description('duplicate'::regclass, 'pg_class'), obj_description('duplicate'::regclass, 'pg_proc'),
       obj_description('bar'::regproc, 'pg_proc');
select c.relname, d.description from pg_class c
  join pg_description d on d.objoid = c.oid and d.classoid = 'pg_class'::regclass
 where c.relname = 'duplicate';
insert into pg_description (objoid, classoid, objsubid, description)
  values ('duplicate'::regclass, 'pg_class'::regclass, 1, 'column x');
select col_description('duplicate'::regclass, 1);
update pg_description set description = description || '!' where objoid = 'duplicate'::regclass;
select classoid::regclass, objsubid, description from pg_description
 where objoid = 'duplicate'::regclass order by 2, 3;
delete from pg_description where objoid = 'duplicate'::regclass and classoid = 'pg_proc'::regclass;
select classoid::regclass, objsubid, description from pg_description
 where objoid = 'duplicate'::regclass order by 2, 3;
-- A built-in function's comment, removed and given back.
delete from pg_description where objoid = 'pg_catalog.abs(int4)'::regprocedure;
select obj_description('pg_catalog.abs(int4)'::regprocedure, 'pg_proc');
insert into pg_description values ('pg_catalog.abs(int4)'::regprocedure, 'pg_proc'::regclass, 0, 'absolute');
select obj_description('pg_catalog.abs(int4)'::regprocedure, 'pg_proc');
-- Its unique index and NOT NULL columns.
insert into pg_description values ('pg_catalog.abs(int4)'::regprocedure, 'pg_proc'::regclass, 0, 'again');
update pg_description set description = null where objoid = 'pg_catalog.abs(int4)'::regprocedure;
update pg_description set objsubid = 1 where objoid = 'pg_catalog.abs(int4)'::regprocedure;
select objsubid, description from pg_description where objoid = 'pg_catalog.abs(int4)'::regprocedure;
drop table duplicate;
drop function bar();

-- pgjdbc's updatable result set writes pg_class.relname back unchanged.
prepare relname_upd(name, oid) as update pg_class set relname = $1 where oid = $2;
execute relname_upd('pg_class', 'pg_class'::regclass);
select relname from pg_class where oid = 'pg_class'::regclass;
deallocate relname_upd;

-- unnest of an array of a one-field composite in FROM: the field (Hibernate).
create type onefield as (x int);
create type twofield as (x int, y text);
select * from unnest(array[row(1)::onefield, row(2)::onefield]);
select * from unnest(array[row(1)::onefield]) as q;
select q.x from unnest(array[row(3)::onefield]) q;
select * from unnest(array[row(1)::onefield], array['a']);
select * from unnest(array[row(1, 'a')::twofield]);
select unnest(array[row(1)::onefield]);
drop type onefield, twofield;

-- INSERT ... SELECT: an untyped parameter of the select list takes its column's type under
-- ORDER BY / LIMIT too (Drizzle).
create table ins_src (id int, n text);
create table ins_dst (a int, b varchar(20));
insert into ins_src values (2, 'b'), (1, 'a');
prepare ins_p as insert into ins_dst (a, b) select id, $1 from ins_src where id > 0 order by id;
prepare ins_q as insert into ins_dst (a, b) select id, $1 from ins_src limit 1;
select name, parameter_types from pg_prepared_statements where name like 'ins_%' order by 1;
execute ins_p('x');
select * from ins_dst order by a;
deallocate ins_p;
deallocate ins_q;
drop table ins_src, ins_dst;

-- btree_gist: varchar keys use gist_text_ops (TypeORM's exclusion constraints).
create extension btree_gist;
create table gv (a varchar(10), b int, c char(3));
create index gv_a on gv using gist (a);
alter table gv add constraint gv_ex exclude using gist (a with =, b with <>);
insert into gv values ('x', 1, 'a');
insert into gv values ('x', 2, 'a');
insert into gv values ('y', 2, 'a');
select i.indexrelid::regclass, k.n, o.opcname
  from pg_index i, unnest(i.indclass::oid[]) with ordinality k(c, n), pg_opclass o
 where i.indrelid = 'gv'::regclass and o.oid = k.c order by 1, 2;
drop table gv;
drop extension btree_gist;

-- information_schema.columns: an extension's type is USER-DEFINED, in its schema (TypeORM).
create extension citext;
create table ext_cols (a citext, b citext[], c text);
select column_name, data_type, udt_schema, udt_name from information_schema.columns
 where table_name = 'ext_cols' order by 1;
drop table ext_cols;
drop extension citext;

-- GUC_IS_NAME parameters are truncated to 63 bytes, with a NOTICE (tokio-postgres).
set application_name = 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx';
show application_name;
reset application_name;

-- information_schema.columns: a renamed enum's current name (TypeORM's loadTables casts
-- udt_schema.udt_name to regtype), a row type's schema, a column's collation.
create type renamed_enum as enum ('A', 'B');
create table enum_cols (e renamed_enum, ea renamed_enum[], n varchar(10) collate "POSIX", m text collate "C", o text);
alter type renamed_enum rename to renamed_enum_old;
select column_name, data_type, udt_schema, udt_name, ('"' || udt_schema || '"."' || udt_name || '"')::regtype,
       collation_catalog is not null as has_catalog, collation_schema, collation_name
  from information_schema.columns where table_name = 'enum_cols' order by 1;
select udt_schema, udt_name from information_schema.columns
 where table_name = 'pg_statistic_ext_data' and column_name = 'stxdexpr';
drop table enum_cols;
drop type renamed_enum_old;

-- A GROUP BY key that is a sub-select over a join, by its ordinal (Django's aggregation
-- test_aggregation_subquery_annotation_multivalued).
create table gk_a (id int primary key, name text);
create table gk_b (id int primary key, name text);
insert into gk_a values (1, 'x'), (2, 'y');
insert into gk_b values (1, 'x');
select gk_a.id, (select u0.id from gk_a u0, gk_b u2 where u2.name = gk_b.name and u0.name = u2.name) as s
  from gk_a, gk_b group by gk_a.id, 2 order by 1;
select count(*) from (select gk_a.id, (select u0.id from gk_a u0, gk_b u2 where u0.id = gk_a.id and u2.name = gk_b.name) as s
  from gk_a left join gk_b on gk_b.id = gk_a.id group by gk_a.id, 2) q;
drop table gk_a, gk_b;

-- LIKE: a pattern ending in its escape character fails only when matching reaches it
-- (pgjdbc's DatabaseMetaDataTest.escaping).
select 'b' like 'a\', 'a' like 'a\', '' like '\';
select 'ab' like 'a\';
select 'ab' like '%\';
create table "a'" (x int);
select relname from pg_class c where c.relname like 'a\' and c.relkind = 'r';
drop table "a'";

-- A FROM subquery's own FOR UPDATE over its outer join's nullable side (TypeORM's findOne
-- with relations and a lock).
create table lk_user (id int primary key, name text);
create table lk_post (id int primary key, author int references lk_user);
insert into lk_user values (1, 'a'); insert into lk_post values (1, 1);
select distinct s.pid from (select p.id as pid, u.name from lk_post p left join lk_user u on u.id = p.author
  where p.id = 1 for update) s order by 1 limit 1;
select distinct s.pid from (select p.id as pid, u.name from lk_post p left join lk_user u on u.id = p.author
  where p.id = 1 for update of p) s order by 1 limit 1;
select s.pid from (select p.id as pid from lk_post p left join lk_user u on u.id = p.author for share of u) s;
drop table lk_post, lk_user;
