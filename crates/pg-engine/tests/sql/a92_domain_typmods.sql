-- Domains over domains and columns declared as domains: the type modifiers the catalogs
-- show (typtypmod, typndims, atttypmod), format_type over them (what pg_dump prints), and
-- information_schema.columns.
create domain mynums as numeric(4,2)[1];
create domain mynums2 as mynums;
create domain vc5 as varchar(5);
create domain vc5b as vc5;
create domain vc5c as vc5b not null;
create domain ts3 as timestamp(3);
create domain ts3b as ts3 check (value > '2000-01-01');
create domain posint as int check (value > 0);
create domain posints as posint[];
create table dd (a mynums, b mynums2, c vc5, d vc5b, e vc5c, f vc5b[], g ts3b, h posints, i varchar(7), j numeric(6,1)[]);
select typname, typtypmod, format_type(typbasetype, typtypmod), format_type(oid, typtypmod), typndims
from pg_type where typname in ('mynums', 'mynums2', 'vc5', 'vc5b', 'vc5c', 'ts3', 'ts3b', 'posint', 'posints')
order by 1;
select attname, atttypmod, format_type(atttypid, atttypmod), attndims
from pg_attribute where attrelid = 'dd'::regclass and attnum > 0 order by attnum;
-- pg_dump's queries: a domain's definition, a table's columns.
select t.typname, t.typnotnull, format_type(t.typbasetype, t.typtypmod) as typdefn
from pg_type t where t.typtype = 'd' and t.typnamespace = 'public'::regnamespace order by 1;
select domain_name, data_type, character_maximum_length, numeric_precision, numeric_scale,
  datetime_precision, udt_name
from information_schema.domains where domain_schema = 'public' order by 1;
select column_name, data_type, character_maximum_length, character_octet_length,
  numeric_precision, numeric_precision_radix, numeric_scale, datetime_precision,
  domain_name, udt_schema, udt_name
from information_schema.columns where table_name = 'dd' order by ordinal_position;
-- Views and composite types over such columns.
create view ddv as select b, d, e::vc5b as e2, d::vc5 as d2, c::varchar(3) as c3, i from dd;
select attname, atttypmod, format_type(atttypid, atttypmod)
from pg_attribute where attrelid = 'ddv'::regclass order by attnum;
create type ddt as (x mynums2, y vc5, z vc5b, w varchar(4));
select attname, atttypmod, format_type(atttypid, atttypmod)
from pg_attribute where attrelid = 'ddt'::regclass order by attnum;
insert into dd values ('{1.5}', '{2.25}', 'ab', 'cd', 'ef', '{gh}', '2020-01-01 10:00:00.12345', '{3}', 'ij', '{1.25}');
select a, b, c, d, e, f, g, h, i, j, pg_typeof(b), pg_typeof(d), pg_typeof(d::vc5) from dd;
select * from ddv;
-- The domain's typmod still applies to values through every level.
insert into dd (b) values ('{123.456}');
insert into dd (d) values ('abcdefg');
select ('2021-02-03 04:05:06.789123'::ts3b)::text;
drop view ddv;
drop table dd;
drop type ddt;
drop domain posints, posint, ts3b, ts3, vc5c, vc5b, vc5, mynums2, mynums;
