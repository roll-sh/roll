-- A value's declared domain type survives through columns, subqueries, views, CTEs,
-- function results, subscripts, casts and set operations.
create domain di_posint as int check (value > 0);
create table di_t (c di_posint, d di_posint[]);
insert into di_t values (1, '{1,2}');
select pg_typeof(c), pg_typeof(d), pg_typeof(d[1]), pg_typeof(d[1:1]), pg_typeof(array[c]), pg_typeof(array[1::di_posint]) from di_t;
select pg_typeof(x) from (select c from di_t) s(x);
select pg_typeof(c) from (select c from di_t union all select c from di_t) s;
select pg_typeof(c) from (select c from di_t union all select 1) s;
create view di_v as select c, c + 1 as e from di_t;
select pg_typeof(c), pg_typeof(e) from di_v;
select pg_typeof(c) from di_t group by c;
select pg_typeof(max(c)) from di_t;
with w as (select c from di_t) select pg_typeof(c) from w;
select pg_typeof(r.c) from di_t r;
select pg_typeof(f), pg_typeof(g) from (values (1::di_posint, 1::di_posint), (2::di_posint, 2)) v(f, g);
select f from (values (1::di_posint), (2::di_posint)) v(f) order by 1;
select pg_typeof(c::di_posint), pg_typeof(c::int), pg_typeof(c::int8) from di_t;
select pg_typeof(coalesce(c, c)), pg_typeof(coalesce(c, 1)), pg_typeof(greatest(c, c)), pg_typeof(nullif(c, 5)) from di_t;
select pg_typeof(case when c > 0 then c else c end), pg_typeof(case when c > 0 then c end) from di_t;
select a.c, pg_typeof(b.c) from di_t a join di_t b using (c);
create function di_make(int) returns di_posint as 'select $1' language sql;
select di_make(3), pg_typeof(di_make(3)), pg_typeof(di_make(3) + 1);
select pg_typeof(coalesce(di_make(3), 1::di_posint));
select di_make(-1);
select pg_typeof(m) from di_make(4) m;
-- functions declared on the domain are chosen for values of it
create function di_f(di_posint) returns text as 'select ''dom''' language sql;
create function di_f(int) returns text as 'select ''int''' language sql;
select di_f(c), di_f(c::int), di_f(1), di_f(1::di_posint) from di_t;
select di_f(x) from (select c from di_t) s(x);
-- CREATE TABLE AS keeps it
create table di_ctas as select c, d, c + 0 as p from (select * from di_t) s;
select attname, atttypid::regtype from pg_attribute where attrelid = 'di_ctas'::regclass and attnum > 0 order by attnum;
-- a domain over an array of a domain
create domain di_dpa as di_posint[];
create domain di_dpa2 as di_dpa check (cardinality(value) < 3);
select array[-1]::di_dpa;
select array[1,2,3]::di_dpa2;
select pg_typeof(array[1]::di_dpa2), pg_typeof((array[1]::di_dpa2)[1]);
select typname, typbasetype::regtype, typelem::regtype, typarray::regtype from pg_type where typname in ('di_dpa', 'di_dpa2', '_di_dpa2') order by 1;
-- arrays of it are one-dimensional arrays of array values
create table di_a (f1 di_dpa[], f2 di_dpa, f3 di_posint[]);
insert into di_a values (array[array[42]]);
insert into di_a values (array[array[42]::di_posint[]]);
insert into di_a values (array[array[1]::di_dpa], array[2], array[3]);
select pg_typeof(f1), pg_typeof(f1[1]), pg_typeof(f1[1][1]), pg_typeof((f1[1])[1]), pg_typeof(f2), pg_typeof(f2[1]), pg_typeof(f3), pg_typeof(f3[1]) from di_a;
select f1, f2, f3, f1[1], f2[1], f3[1], (f1[1])[1] from di_a;
select pg_typeof(f2[1:1]), pg_typeof(f3[1:1]), pg_typeof(f1[1:1]) from di_a;
update di_a set f1[2] = array[99];
update di_a set f1[3] = array[-1];
update di_a set f1[2][1] = array[97];
select f1, array_dims(f1), array_length(f1, 1), cardinality(f1), array_ndims(f1) from di_a;
select x, pg_typeof(x) from di_a, unnest(f1) x;
select array_agg(f2) from di_a;
select f1 || array[5]::di_dpa from di_a;
select array_append(f1, array[6]::di_dpa) from di_a;
select f1::text, '{"{1}","{2,3}"}'::di_dpa[] from di_a;
select f1 = f1, f1 < '{"{50}"}'::di_dpa[] from di_a;
select row_to_json(di_a) from di_a;
select to_json(f1) from di_a;
select array[array[42]::di_dpa, array[1,2]::di_dpa];
select (array[array[42]::di_dpa, array[1,2]::di_dpa])[2];
select array_dims(array[array[42]::di_dpa, array[1,2]::di_dpa]);
select pg_typeof(array[array[42]::di_dpa]);
select '{"{1,2}","{3}"}'::di_dpa[], pg_typeof('{"{1,2}","{3}"}'::di_dpa[]);
create function di_g(di_dpa) returns text as 'select ''dom''' language sql;
create function di_g(int[]) returns text as 'select ''arr''' language sql;
select di_g(array[1]::di_dpa), di_g(array[1]);
select di_g(x) from unnest(array[array[42]::di_dpa]) x;
-- dropping the element domain takes the domains over its array with it
drop table di_a, di_ctas;
drop domain di_posint cascade;
-- a function returning a composite (or a domain over one) in FROM: its columns
create type di_comp as (r float8, i float8);
create domain di_dcomp as di_comp check ((value).r <= (value).i);
create function di_mkd(r float8, i float8) returns di_dcomp as 'select row(r, i)' language sql;
create function di_mk(r float8, i float8) returns di_comp as 'select row(r, i)' language sql;
select di_mkd(1,2);
select di_mkd(2,1);
select * from di_mkd(1,2) m;
select * from di_mkd(2,1) m;
select m is not null, (m).r from di_mkd(1,2) m;
select * from di_mk(1,2) m;
select r from di_mk(3,4);
-- information_schema.check_constraints shows domain constraints as deparsed
create domain di_c as int not null check (value > 0) check (value < 10 and value <> 5);
select constraint_name, check_clause from information_schema.check_constraints where constraint_name like 'di_c%' order by 1;
