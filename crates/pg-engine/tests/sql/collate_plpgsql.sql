-- PL/pgSQL variables take the call's input collation (fn_input_collation), unless declared
-- with COLLATE.
create or replace function f(a text, b text) returns boolean language plpgsql as $$ begin return a < b; end $$;
create or replace function g(a text) returns text language plpgsql as $$ declare l text := 'B'; begin return (select string_agg(x, ',' order by x) from (values (a), (l), ('b'), ('a')) v(x)); end $$;
create or replace function h(a text) returns text language plpgsql as $$ declare l text collate "C" := 'B'; begin return (l < 'a')::text || (a < 'B')::text; end $$;
create or replace function u(a text) returns text language plpgsql as $$ begin return upper(a) || lower(a) || initcap(a); end $$;
create or replace function e(a text) returns boolean language plpgsql as $$ begin return a = 'abc'; end $$;
create collation nd (provider=icu, locale='und-u-ks-level2', deterministic=false);
create table t (x text collate "sv-x-icu");
insert into t values ('ä'),('z');
select f('a', 'B'), f('a' collate "C", 'B'), f('a' collate "en-x-icu", 'B');
select g('A'), g('A' collate "C"), g('A' collate "en-x-icu");
select h('a'), h('a' collate "en-x-icu");
select u('ıstanbul' collate "tr-x-icu"), u('ıstanbul');
select e('ABC'), e('ABC' collate nd);
select x, f(x, 'z') from t order by x;
select f('a' collate "C", 'B' collate "en-x-icu");
create or replace function s(a text) returns text language plpgsql as $$ declare r text; begin select string_agg(x, ',' order by x) into r from (values (a), ('b'), ('B')) v(x); return r || (select min(y) from (values (a), ('Z')) w(y)); end $$;
select s('a' collate "en-x-icu"), s('a');
create or replace function q(a text) returns text language plpgsql as $$ declare l text collate pg_catalog."C" := 'B'; begin return l < 'a'; end $$;
select q('x' collate "en-x-icu");
select * from g('A' collate "en-x-icu");
create or replace function n(a int, b text) returns boolean language plpgsql as $$ begin return b < 'B'; end $$;
select n(1, 'a' collate "en-x-icu"), n(1, 'a');
select * from f('a' collate "C", 'B' collate "en-x-icu");
