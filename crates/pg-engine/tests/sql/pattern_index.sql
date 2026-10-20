-- LIKE, ILIKE, SIMILAR TO and regular expressions with a fixed prefix become btree index
-- conditions (a range, or = for an exact pattern; text_pattern_ops uses ~>=~ / ~<~), the
-- pattern itself still checked as a filter; patternsel's estimates (histogram matches, MCVs,
-- prefix ranges); bpchar and name columns; a histogram end replaced by the column's actual
-- minimum or maximum read through an index (get_actual_variable_range).
set jit = off;
create table pi_l (id int, t text, v varchar(20), n name, b char(8));
insert into pi_l select g, 'word' || (g % 500) || 'x', 'v' || (g % 300), 'nm' || g, 'b' || (g % 50) from generate_series(1, 20000) g;
create index pi_l_t on pi_l (t);
create index pi_l_v on pi_l (v);
create index pi_l_n on pi_l (n);
create index pi_l_b on pi_l (b);
analyze pi_l;
explain select * from pi_l where t like 'word1%';
explain select * from pi_l where t like 'word12%';
explain select * from pi_l where t like 'word123x';
explain select * from pi_l where t like '%word1';
explain select * from pi_l where t like 'word1_x';
explain select * from pi_l where t ilike 'word1%';
explain select * from pi_l where t ilike '12%';
explain select * from pi_l where t ~ '^word12';
explain select * from pi_l where t ~ '^word12x$';
explain select * from pi_l where t ~ 'word12';
explain select * from pi_l where t ~ '^wor[d]12';
explain select * from pi_l where t ~ '^w(o)rd12';
explain select * from pi_l where t ~ '^word1(2|3)';
explain select * from pi_l where t ~ '^(word12)$';
explain select * from pi_l where t ~* '^word12';
explain select * from pi_l where t similar to 'word12%';
explain select * from pi_l where t not like 'word1%';
explain select * from pi_l where v like 'v1%';
explain select * from pi_l where n like 'nm12%';
explain select * from pi_l where b like 'b1%';
explain select * from pi_l where b = 'b1';
explain select * from pi_l where t like 'zzz%';
explain select * from pi_l where t like 'word%';
explain select * from pi_l where t like 'word42%' and id < 100;
explain select t from pi_l where t like 'word42%';
select count(*) from pi_l where t like 'word42%';
select count(*) from pi_l where t ~ '^word12';
select count(*) from pi_l where t similar to 'word12%';
select count(*) from pi_l where t ilike 'WORD4%';
select count(*), min(t), max(t) from pi_l where t like 'word12%';
-- histogram ends replaced by the actual minimum / maximum
explain select * from pi_l where t < 'word';
explain select * from pi_l where t > 'word99x';
explain select * from pi_l where t >= 'word1';
explain select * from pi_l where id > 19990;
explain select * from pi_l where id < 5;
create index pi_l_id on pi_l (id);
explain select * from pi_l where id > 19990;
explain select * from pi_l where id < 5;
insert into pi_l select g, 'zzz' || g, 'v', 'n', 'b' from generate_series(20001, 20500) g;
explain select * from pi_l where t > 'zz';
explain select * from pi_l where t like 'zzz1%';
explain select * from pi_l where id > 20000;
-- text_pattern_ops
create table pi_p (id int, t text);
insert into pi_p select g, 'word' || (g % 500) || 'x' from generate_series(1, 20000) g;
create index pi_p_t on pi_p (t);
create index pi_p_tp on pi_p (t text_pattern_ops);
analyze pi_p;
explain select * from pi_p where t like 'word1%';
explain select * from pi_p where t like 'word42%';
explain select * from pi_p where t ~ '^word12';
explain select * from pi_p where t ~<~ 'word2';
explain select * from pi_p where t ~>=~ 'word42' and t ~<~ 'word43';
explain select * from pi_p where t < 'word2';
