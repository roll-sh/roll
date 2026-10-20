-- standard_conforming_strings = off: backslash escapes in ordinary literals, the scanner's
-- warnings (escape_string_warning) and backslash_quote, and what prints string literals.
set standard_conforming_strings = off;
select 'a\nb', 'a\\b', 'it\'s', 'x\tq', 'a''b', 'no esc';
select 'a\\b' as x, 'c\d' as y;
select E'a\\b', 'a\\b' like 'a\\\\b', 'a\b' like 'a\\b';
select quote_literal('a\b'), quote_literal(E'a\\b');
set escape_string_warning = off;
select 'a\\b';
set escape_string_warning = on;
set backslash_quote = off;
select 'it\'s';
select E'it\'s';
set backslash_quote = on;
select 'it\'s';
set backslash_quote = safe_encoding;
create view gxw_v as select 'a\\b'::text as c, 'x''y' as d, E'\t' as e;
select pg_get_viewdef('gxw_v'::regclass);
set standard_conforming_strings = on;
select pg_get_viewdef('gxw_v'::regclass);
select * from gxw_v;
drop view gxw_v;
show standard_conforming_strings;
set standard_conforming_strings = off;
select '\u0041', 'ab\u0041\n', 'a\U00000042', '\x41\x42', E'\u0041';
select 'a'
  'b\n'   'c';
select 'a'
'b\n';
select 'n', b'101', x'1f', 'a' -- c
'q\\';
create function gxw_f1() returns text language plpgsql as $$
begin
  return 'a\\b' || 'c\nd';
end $$;
select gxw_f1();
create function gxw_f2() returns text language sql as $$ select 'x\\y' $$;
select gxw_f2();
create function gxw_f3() returns text language sql begin atomic select 'p\\q'; end;
select gxw_f3();
select pg_get_functiondef('gxw_f3'::regproc);
set standard_conforming_strings = on;
select gxw_f1(), gxw_f2(), gxw_f3();
select pg_get_functiondef('gxw_f3'::regproc);
set standard_conforming_strings = off;
select gxw_f1(), gxw_f2(), gxw_f3();
do $$ begin raise notice '%', 'r\\s'; end $$;
do $$ begin execute 'select ''e\\x'''; end $$;
prepare gxw_p as select 'pp\\x' as c;
execute gxw_p;
select statement from pg_prepared_statements;
select 'a\\b' = 'a\\b' as eq, length('a\\b');
select 'a\\b'::bytea, '\\x41'::bytea;
select ARRAY['a\\b', 'c\"d'];
select '{"a\\\\b"}'::text[];
select 1 /* x 'a\\b' */, 'c\\d' -- 'z\\'
;
drop function gxw_f1(), gxw_f2(), gxw_f3();
reset standard_conforming_strings;
set standard_conforming_strings = off;
select 'a\\b' as x, 'c\d' as y; select 'q\\';
select 'a\\b' + ;
select 'a\\b', 'c\\d' frm;
select 'x\'y', E'z\'w';
set backslash_quote = off;
select 'p\\q', 'it\'s';
select E'it\'s';
select 'ok''s';
set backslash_quote = on;
select 'it\'s';
set backslash_quote = safe_encoding;
select 'it\'s';
select U&'d\0061t';
select U&"d\0061t" from (select 1) x;
select '\xff';
select '\000';
select '\u00e9t\xC3\xA9', length('\u00e9t\xC3\xA9');
select '\uD83D\uDE00', '\U0001F600';
select '\uD83D';
select '\u12';
set escape_string_warning = off;
select 'a\\b', 'c\nd';
set escape_string_warning = on;
select 'a\\b' like 'a\\\\b', 'a\\b' like 'a\\b' escape '';
select quote_literal('x\\y'), quote_ident('a\\b'), format('%L', 'p\\q');
create table gxw_t (a text default 'd\\e', b text check (b <> 'x\\y'));
insert into gxw_t (b) values ('b\\c') returning *;
select pg_get_expr(adbin, adrelid) from pg_attrdef where adrelid = 'gxw_t'::regclass;
select pg_get_constraintdef(oid) from pg_constraint where conrelid = 'gxw_t'::regclass;
set standard_conforming_strings = on;
select pg_get_expr(adbin, adrelid) from pg_attrdef where adrelid = 'gxw_t'::regclass;
insert into gxw_t (b) values ('b\c') returning *;
set standard_conforming_strings = off;
copy gxw_t from stdin;
x\\y	p\tq
\.
select * from gxw_t;
copy gxw_t to stdout;
create function gxw_s1() returns text language sql as 'select ''a\\b''';
select gxw_s1(), prosrc from pg_proc where proname = 'gxw_s1';
create function gxw_s2() returns text language plpgsql as 'begin return ''r\\s''; end';
select gxw_s2();
create view gxw_v2 as select 'v\\w' as c, E'\\' as d where 'k\\' <> '';
select pg_get_viewdef('gxw_v2');
select * from gxw_v2;
create view gxw_v3 as select a from gxw_t where a like 'q\\_%' and b <> 'p''\\';
explain (costs off) select * from gxw_t where a = 'x\\y' and b ~ '\\d';
explain (costs off, verbose) select a || '\\' from gxw_t;
select definition from pg_views where viewname = 'gxw_v3';
drop view gxw_v3;
set standard_conforming_strings = on;
select pg_get_viewdef('gxw_v2');
select * from gxw_v2;
set standard_conforming_strings = off;
do $$ begin raise notice 'msg \\ %', 'arg\\'; end $$;
do $$ declare v text := 'i\\j'; begin raise notice '%', v; end $$;
select 'end\\';
drop view gxw_v2;
drop function gxw_s1(), gxw_s2();
drop table gxw_t;
show standard_conforming_strings;
reset standard_conforming_strings;
select 'a\\b';

-- PL/pgSQL: the compile-time scan and check of each SQL expression warn (twice for literals
-- in expressions), then each expression warns once when first prepared.
set standard_conforming_strings = off;
create function gxw_p1(x text) returns text language plpgsql as $$
declare
  v text := 'd\\e';
begin
  raise notice 'fmt\\ %', x;
  if x = 'c\\d' then
    return v || 'x\ny';
  end if;
  perform 'p\\q';
  return 'it\'s';
end $$;
select gxw_p1('c\\d');
select gxw_p1('z');
select gxw_p1('z');
create function gxw_p2() returns text language plpgsql as 'begin return ''a\\b''; end';
select gxw_p2();
create function gxw_p3() returns int language plpgsql as $$ begin return 'x\\y' + ; end $$;
do $$ declare r text; begin execute format('select %L', 'q\\r') into r; raise notice '%', r; end $$;
set escape_string_warning = off;
create function gxw_p4() returns text language plpgsql as $$ begin return 'w\\x'; end $$;
select gxw_p4();
drop function gxw_p1(text), gxw_p2(), gxw_p4();
do $$
declare
  a int := 1;
  b int := 1/0;
begin
  null;
end $$;
do $$
declare a int := 1; b int := 1/0;
begin
  null;
end $$;
do $$
<<lbl>>
declare
  a int := 1;
begin
  declare
    c int :=
     1/0;
  begin
  end;
end $$;
SELECT U&'wrong: \061';
SELECT U&'wrong: \+0061';
SELECT E'wrong: \u061';
SELECT E'wrong: \U0061';
SET standard_conforming_strings TO off;
SELECT U&'d\0061t\+000061' AS U&"d\0061t\+000061";
SELECT U&'d!0061t\+000061' UESCAPE '!' AS U&"d*0061t\+000061" UESCAPE '*';
SELECT U&' \' UESCAPE '!' AS "tricky";
SELECT 'tricky' AS U&"\" UESCAPE '!';
SELECT U&'wrong: \061';
SELECT U&'wrong: \+0061';
SELECT U&'wrong: +0061' UESCAPE '+';
RESET standard_conforming_strings;
