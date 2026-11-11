-- Case mapping and case-insensitive matching under a collation whose ctype is C (C, POSIX,
-- ucs_basic: ASCII letters only) and the default one (the builtin C.UTF-8 locale: simple
-- Unicode mappings), and pg_c_utf8 like the default.
select lower('ÀÉÎ ÑẞΣ' collate "C"), upper('àéî ñßσς' collate "C"), initcap('àéî ñß hello wORLD' collate "C");
select lower('ÀÉÎ ÑẞΣ'), upper('àéî ñßσς'), initcap('àéî ñß hello wORLD');
select lower('ÀÉÎ' collate "POSIX"), upper('àéî' collate "POSIX"), initcap('éa' collate "POSIX");
select lower('ÀÉÎ' collate "ucs_basic"), upper('àéî ß' collate "ucs_basic"), initcap('éa ß' collate "ucs_basic");
select lower('ÀÉÎ' collate "pg_c_utf8"), upper('àéî ß' collate "pg_c_utf8"), initcap('éa ß ǆ' collate "pg_c_utf8");
select casefold('Àß' collate "C"), casefold('ÀßΣ'), casefold('ÀB' collate "POSIX"), lower(null collate "C");
select initcap('hello-wORLD é3x'), initcap('hello-wORLD é3x' collate "C"), initcap('x9é_y' collate "C");
select lower('ÀB'::varchar collate "C"), upper('àb'::char(3) collate "C");
select lower('ǅ' collate "C"), upper('ǆ' collate "C"), initcap('ǆa' collate "C"), initcap('ǆa');
select lower('İ'), lower('İ' collate "C"), upper('ﬀ'), upper('ﬀ' collate "C");
select upper(lower('ÄÖ' collate "C")), lower('Ä' collate "C") = 'Ä';
-- ILIKE lowers both sides under the collation.
select 'É' collate "C" ilike 'é', 'É' ilike 'é', 'É' collate "C" not ilike 'é', 'É' collate "C" !~~* 'é';
select 'é' collate "C" ilike '_', 'é' collate "C" ilike '__', 'é' ilike '_', 'xÉy' collate "C" ilike 'X%Y', 'İ' ilike 'i', 'İ' collate "C" ilike 'i';
select 'abc'::name ilike 'ABC', 'Éb'::name ilike 'éB', 'Éb'::name ~~* 'éB'::text collate "default";
-- Regular expressions: case-insensitivity and classes follow the collation too.
select 'É' collate "C" ~* 'é', 'É' ~* 'é', 'Éa' collate "C" ~* 'éA', 'aB' collate "C" ~* 'Ab', 'q' collate "C" ~* '[A-Z]', 'Q' collate "C" ~* '[a-c]x|[p-r]';
select 'É' collate "C" ~ '[[:upper:]]', 'É' ~ '[[:upper:]]', 'é' ~ '[[:upper:]]', 'é' ~* '[[:upper:]]', 'é' collate "C" ~ '[[:alpha:]]', 'é' ~ '[[:alpha:]]', 'é' ~ '[[:alnum:]]';
select '٣' ~ '\d', '٣' ~ '\w', '٣' ~ '[[:digit:]]', 'é' collate "C" ~ '\w', 'é' collate "C" ~ '\W', 'é' ~ '\w', 'é' ~ '\W', '_' ~ '\w', 'x' collate "C" ~ '[\w]';
select ']' ~ '[]]', 'a' ~ '[^]]', '[' ~ '[[]', 'x' ~ '[[:alpha:][:digit:]]', E' ' ~ '\s', E' ' collate "C" ~ '\s', E' ' ~ '[[:space:]]';
select 'Ab' collate "C" ~* '[[:lower:]]b', 'É' collate "C" !~* 'é', 'É' collate "C" !~ 'é', 'abc' collate "C" ~ '^a.c$', 'K' ~* 'k', 'K' collate "C" ~* '[a-z]';
-- Columns carry their collation; COLLATE overrides it.
create table gxc_c (a text collate "C", b text, n name);
insert into gxc_c values ('ÀbÇ', 'ÀbÇ', 'ÀbÇ');
select lower(a), upper(a), initcap(a), lower(b), lower(n), lower(a collate "default"), a ilike 'àbç', b ilike 'àbç', n ilike 'àbç', a not ilike 'àbç', a !~~* 'àBç', a ~* 'àbç', b ~* 'àbç' from gxc_c;
select lower(a || b), lower(a) || b, lower(a || b collate "C"), lower(a || n) from gxc_c;
select lower(b || n) from gxc_c;
create view gxc_v as select lower(a) l, a ilike 'x' i, upper(b) u from gxc_c;
select * from gxc_v;
select pg_get_viewdef('gxc_v');
drop view gxc_v;
drop table gxc_c;
