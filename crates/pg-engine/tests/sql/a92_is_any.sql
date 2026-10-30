-- information_schema.columns / .tables read through `table_schema = ANY (array)` (Prisma's
-- introspection): one schema, several, a NULL element, an empty array, with table_name too.
create schema isa1;
create schema isa2;
create table isa1.t (a int, b text default 'x');
create table isa2.t (c numeric(5,2));
create view isa1.v as select a from isa1.t;
select table_schema, table_name, column_name, ordinal_position, data_type
from information_schema.columns where table_schema = any ('{isa1}') order by 1, 2, 4;
select table_schema, table_name, column_name
from information_schema.columns where table_schema = any ('{isa1,isa2}') order by 1, 2, 3;
select count(*) from information_schema.columns where table_schema = any ('{isa1,NULL}');
select count(*) from information_schema.columns where table_schema = any ('{}');
select column_name from information_schema.columns
where table_schema = any (array['isa2']) and table_name = 't' order by 1;
select table_name, table_type from information_schema.tables where table_schema = any ('{isa1}') order by 1;
select count(*) from information_schema.columns where table_schema = any (null::text[]);
drop schema isa1 cascade;
drop schema isa2 cascade;
