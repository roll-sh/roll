-- Row marks in preprocess_rowmarks' order (the locking clauses' relations in the order the
-- clauses name them, then the others' ctids and whole rows in range table order), several
-- locking clauses of one level, whole-row marks' widths (set_rel_width) and the final target
-- list's width of an UPDATE over a join, locked subqueries' scans; constants under a Sort, a
-- constant GROUP BY key, a pulled-up one-row subquery's constants folded.
create table rm1 (a int, b text);
create table rm2 (x int, y text);
create table rm3 (p int, q int);
insert into rm1 select g, 'b' || g from generate_series(1, 10) g;
insert into rm2 select g, 'y' || g from generate_series(1, 10) g;
insert into rm3 select g, g from generate_series(1, 10) g;
analyze rm1, rm2, rm3;

-- A FROM subquery's whole row by its range table place, among the unlocked relations.
explain (verbose, costs off)
select * from (select a, b from rm1 offset 0) s, rm2 where s.a = rm2.x for update of s;
explain (verbose, costs off)
select * from rm2, (select a, b from rm1 offset 0) s where s.a = rm2.x for update of s;
explain (verbose, costs off)
select * from (select a, b from rm1 offset 0) s, rm2, rm3 where s.a = rm2.x and rm3.p = rm2.x for update of rm3;
-- VALUES pulled up: its range table entry after the level's own; a function's in place.
explain (verbose, costs off)
select * from rm2, (values (1), (2)) v(c), generate_series(1, 3) g where v.c = rm2.x and g < rm2.x for update of rm2;
explain (verbose, costs off)
with c as materialized (select * from rm3) select * from rm2, c, rm1 where c.p = rm2.x and rm1.b = rm2.y for update of rm1;
-- The locked relations in the order the clauses name them.
explain (verbose, costs off)
select * from rm3, rm2, rm1 where rm3.p = rm2.x and rm1.a = rm2.x for update of rm1, rm3;
explain (verbose, costs off)
select * from rm3, rm2, rm1 where rm3.p = rm2.x and rm1.a = rm2.x for update of rm1 for share of rm3;
select * from rm3, rm2, rm1 where rm3.p = rm2.x and rm1.a = rm2.x and rm1.a < 3 order by 1 for update of rm1 for share of rm3;
select * from rm3, rm1 where rm3.p = rm1.a and rm1.a < 3 order by 1 for share of rm1 for update of rm1, rm3 skip locked;
-- A data-modifying statement's marks.
explain (verbose, costs off)
update rm1 set b = 'z' from rm3, (select x from rm2 offset 0) s where s.x = rm1.a and rm3.p = rm1.a and rm3.q < 3;
explain (verbose, costs off)
update rm1 set b = 'z' from (values (1), (2)) v(c), rm3 where v.c = rm1.a and rm3.p < rm1.a;

-- Widths: a whole-row mark's (24 bytes and its columns'), and a locked subquery's scan.
explain select * from (select x from rm2 offset 0) s, rm1 where s.x = rm1.a for update of rm1;
explain select * from (select x from rm2 for update) s, rm1 where s.x = rm1.a;
explain select * from (select x from rm2 offset 0 for update) s, rm1 where s.x = rm1.a;
explain select * from (select x, y from rm2 offset 0) s, rm1 where s.x = rm1.a for update of s;
-- An UPDATE over a join: the new values, the ctids, the whole rows.
explain update rm1 set b = 'z' from (select x from rm2 offset 0) s where s.x = rm1.a;
explain update rm1 set b = b || 'z' from rm3 where rm3.p = rm1.a;
explain update rm1 set b = rm3.q::text from rm3 where rm3.p = rm1.a;
explain delete from rm1 using rm3 where rm3.p = rm1.a;
explain update rm1 set a = a + 1 from (values (1), (2)) v(c) where v.c = rm1.a;
explain update rm1 set a = a + 1 from (select * from rm3 limit 5) s where s.p = rm1.a;
begin;
update rm1 set b = 'z' from (select x from rm2 offset 0) s, rm3 where s.x = rm1.a and rm3.p = rm1.a and rm1.a < 3 returning rm1.*, s;
rollback;

-- Constants: under a Sort, as GROUP BY keys, a pulled-up one-row subquery's folded.
explain (verbose, costs off) select 'a'::text, a from rm1 order by a;
explain (verbose, costs off) select 1, a from rm1 order by 2;
explain (verbose, costs off) select 'a', count(*) from rm1 group by 1;
explain (verbose, costs off) select count(*) from rm1 group by 1 + 1;
select 'a', count(*) from rm1 group by 1;
select count(*) from rm1 where a < 0 group by 1 + 1;
explain (verbose, costs off) select b || 'y' from (select 'x' b) s;
explain (verbose, costs off) select a + b from (values (1, 2)) t(a, b);
explain (verbose, costs off) select s.b + rm1.a from (select 2 b) s, rm1 where rm1.a > s.b;
select b || 'y' from (select 'x' b) s;
select a + b from (values (1, 2)) t(a, b);
drop table rm1, rm2, rm3;
