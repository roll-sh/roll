-- Case-insensitive matching and case mapping under the default (builtin C.UTF-8)
-- collation use simple case mapping; under a C collation (a name, COLLATE "C", a C
-- column, also through casts, subqueries and views) they only know ASCII.
select 'ſ' ~* 's', 's' ~* 'ſ', 'S' ~* 'ſ', 'ǅ' ~* 'ǅ', 'ǆ' ~* 'ǅ', 'K' ~* 'k', 'k' ~* 'K', 'ß' ~* 'ẞ', 'ẞ' ~* 'ß';
select 'ſ' ilike 's', 's' ilike 'ſ', 'K' ilike 'k', 'ǅ' ilike 'ǆ', 'ß' ilike 'ẞ';
select 'É' ~* '[é]', 'É' ~* '[à-ê]', 'Σ' ~* 'ς', 'ς' ~* 'Σ', 'σ' ~* 'ς';
select upper(name 'àb'), lower(name 'ÀB'), initcap(name 'àb cd'), upper('àb'::name::text), upper(('àb'::name)::text collate "default");
select upper(n), upper(n || 'x'), upper(n::text) from (select 'àb'::name n) s;
select n ~* 'À', n::text ~* 'À', n ilike 'À%', n::text ilike 'À%' from (select 'àb'::name n) s;
select regexp_replace('àbÀB', 'à', 'x', 'gi'), regexp_replace('àbÀB' collate "C", 'à', 'x', 'gi');
select regexp_match('àbÀB' collate "C", '[[:alpha:]]+'), regexp_match('àbÀB', '[[:alpha:]]+');
select regexp_matches('àbÀB' collate "C", 'B', 'gi'), regexp_like('À' collate "C", 'à', 'i'), regexp_count('àÀaA' collate "C", 'a', 1, 'i'), regexp_instr('àÀaA' collate "C", 'A', 1, 1, 0, 'i'), regexp_substr('àÀaA' collate "C", 'A', 1, 1, 'i');
select regexp_split_to_array('aÀbàc' collate "C", 'à', 'i'), regexp_split_to_table('aÀbàc' collate "C", '[[:alpha:]]');
select regexp_like('é' collate "C", '\w'), regexp_like('é', '\w'), 'é' collate "C" ~ '\w', regexp_like('é' collate "C", '[[:upper:]]', 'i');
select substring('àbÀB' collate "C" from '[[:alpha:]]+'), substring('àbÀB' from '[[:alpha:]]+');
select 'ǅ' ~* '[ǅ]', 'Ǆ' ~* '[ǅ]', 'x' ~* '[ǅ-ǅ]', 'ǅ' ~* '[ǅ-ǆ]';
select collation for ('abc'::text::name), collation for ('àb'::name::text), collation for (upper('àb'::name)), collation for ('abc'::name), collation for ('a'::name || 'b');
select upper('àb'::text::name), upper(('àb'::text::name)::text);
create table rc_tc (a text collate "C", b text);
insert into rc_tc values ('àbÀB', 'àbÀB');
select upper(a), upper(b), a ~* 'À', b ~* 'À', regexp_replace(a, 'à', 'x', 'gi'), regexp_replace(b, 'à', 'x', 'gi') from rc_tc;
select * from regexp_matches((select a from rc_tc), '[[:alpha:]]', 'g');
select * from regexp_split_to_table((select a from rc_tc), '[[:alpha:]]');
select x from rc_tc, regexp_split_to_table(a, 'b', 'i') x;
select regexp_matches(a, '.', 'gi') from rc_tc;
select a similar to '%À%', a similar to '%[[:upper:]]%' , substring(a from '[[:upper:]]'), substring(b from '[[:upper:]]') from rc_tc;
drop table rc_tc;
select 'ÀB' ~ '\w+$', 'ÀB' collate "C" ~ '\w+$', 'ÀB' collate "C" ~ '^\W', 'aB' collate "C" ~* '^[[:lower:]]+$', 'ÀB' collate "C" ~ '[[:<:]]B';
select regexp_replace('Ab Cd', '\m(\w)', '<\1>', 'g'), regexp_replace('Ab Cd' collate "C", '[[:<:]]', '|', 'g');
create table rc_tc (a text collate "C", b text);
insert into rc_tc values ('àbÀB', 'àbÀB');
select upper(x), x ~* 'À' from (select a x from rc_tc) s;
select upper((select a from rc_tc)), (select a from rc_tc) ~* 'À', upper((select b from rc_tc));
create view rc_v as select a, b from rc_tc;
select upper(a), upper(b) from rc_v;
with w as (select a from rc_tc) select upper(a) from w;
drop view rc_v;
drop table rc_tc;
