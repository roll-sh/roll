-- The libc provider: musl (the reference server's C library) treats every locale but C and
-- POSIX alike: byte order, its own Unicode classes and simple case mappings.
create collation gxc_en (provider = libc, locale = 'en_US.utf8');
create collation gxc_de (lc_collate = 'de_DE.utf8', lc_ctype = 'de_DE.utf8');
create collation gxc_mixed (provider = libc, lc_collate = 'C', lc_ctype = 'tr_TR.utf8');
create collation gxc_cc (provider = libc, lc_collate = 'en_US.utf8', lc_ctype = 'C');
select collname, collprovider, collisdeterministic, collencoding, collcollate, collctype, colllocale, collversion from pg_collation where collname like 'gxl%' order by 1;
select pg_collation_actual_version(oid) from pg_collation where collname like 'gxl%' order by collname;
select upper('àéî ñß ǆ ǈ ı i ſ ﬁ ŉ ΐ ᾳ' collate gxc_en), lower('ÀÉÎ Ñ ẞ Ǆ ǅ İ I Σ ΑΣ' collate gxc_en);
select initcap('hello wORLD ǆa ﬁx éa 3x a1b don''t' collate gxc_en), initcap('ǆa' collate gxc_cc);
select upper('àéî ß' collate gxc_mixed), upper('àéî ß' collate gxc_cc), lower('ÀÉÎ' collate gxc_cc), casefold('ÀßΣ' collate gxc_en);
select x, x < 'b' collate gxc_en as lt_en, x < 'b' collate gxc_de as lt_de from (values ('a'), ('B'), ('ä'), ('Ä'), ('b'), ('é')) v(x) order by x collate gxc_en;
select string_agg(x, ',' order by x collate gxc_de) from (values ('a'), ('B'), ('ä'), ('Ä'), ('b'), ('é'), ('1'), ('-')) v(x);
select 'ÉCOLE' ilike 'école' collate gxc_en, 'ÉCOLE' ilike 'école' collate gxc_cc, 'İ' ilike 'i' collate gxc_en, 'ß' ilike 'ẞ' collate gxc_en;
select x, x ~ '[[:alpha:]]' collate gxc_en as alpha, x ~ '[[:upper:]]' collate gxc_en as upper, x ~ '[[:lower:]]' collate gxc_en as lower,
       x ~ '[[:digit:]]' collate gxc_en as digit, x ~ '[[:punct:]]' collate gxc_en as punct, x ~ '[[:space:]]' collate gxc_en as space,
       x ~ '[[:graph:]]' collate gxc_en as graph, x ~ '[[:print:]]' collate gxc_en as print
  from (values ('a'), ('É'), ('ß'), ('٣'), ('²'), ('€'), ('_'), ('-'), (' '), (U&'\00A0'), (U&'\3000'), ('ǅ'), ('ⅷ'), ('Ⅷ'), ('中'), ('🙂'), (U&'\0085'), (U&'\00AD'), (U&'\200B')) v(x) order by x collate "C";
select 'É' ~* 'é' collate gxc_en, 'ǅ' ~* 'ǆ' collate gxc_en, 'ſ' ~* 's' collate gxc_en, 'K' ~* 'k' collate gxc_en, 'ß' ~* 'ẞ' collate gxc_en, 'Σ' ~* 'ς' collate gxc_en;
select regexp_replace('ÀBC àbc' collate gxc_en, '[[:upper:]]+', '_', 'g'), regexp_matches('aÉ1٣ ǅ' collate gxc_en, '\w+', 'g');
create table gxc_lt (a text collate gxc_en);
insert into gxc_lt values ('b'), ('A'), ('ä'), ('a');
select a, upper(a), a ~ '^[[:lower:]]' from gxc_lt order by a;
select min(a), max(a) from gxc_lt;
drop table gxc_lt;
create collation gxc_bad (provider = libc, locale = 'en_US.utf8', deterministic = false);
create collation gxc_bad (provider = libc, locale = 'C', rules = '&a < b');
drop collation gxc_en, gxc_de, gxc_mixed, gxc_cc;
-- builtin locales
create collation gxc_b1 (provider = builtin, locale = 'C');
create collation gxc_b2 (provider = builtin, locale = 'C.UTF-8');
create collation gxc_b3 (provider = builtin, locale = 'PG_UNICODE_FAST');
select upper('ß ǆ ﬁ ŉ' collate gxc_b1), upper('ß ǆ ﬁ ŉ' collate gxc_b2), upper('ß ǆ ﬁ ŉ' collate gxc_b3);
select initcap('ǆa ßa ﬁa 3a a3a' collate gxc_b2), initcap('ǆa ßa ﬁa 3a a3a' collate gxc_b3), casefold('ẞ ß İ ſ' collate gxc_b2), casefold('ẞ ß İ ſ' collate gxc_b3);
select '٣' ~ '\d' collate gxc_b2, '٣' ~ '\d' collate gxc_b3, '€' ~ '[[:punct:]]' collate gxc_b2, '€' ~ '[[:punct:]]' collate gxc_b3;
select collname, colllocale, collversion, pg_collation_actual_version(oid) from pg_collation where collname like 'gxc_b%' order by 1;
drop collation gxc_b1, gxc_b2, gxc_b3;
