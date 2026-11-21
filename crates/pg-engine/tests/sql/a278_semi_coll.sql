-- agent 278: a FROM subquery whose WHERE has an IN / EXISTS sub-select is pulled up, its semi
-- join planned once at the level above (a side unique-ified under a merge join's Sort is
-- sorted or hashed whole, its index order unused: create_unique_path); an index's collation
-- matches a comparison's by identity (IndexCollMatchesExprColl): in a C database an index on
-- `t COLLATE "C"` serves no comparison under the default collation, and serves one written
-- COLLATE "C".
create table a278t (u1 int, u2 int, h int, th int) with (autovacuum_enabled = off);
insert into a278t select g, (g * 7) % 10000, g % 100, g % 1000 from generate_series(0, 9999) g;
create unique index a278t_u1 on a278t (u1);
create index a278t_h on a278t (h);
create index a278t_th on a278t (th);
analyze a278t;
explain select count(*) from (select a.u2 from a278t a where u1 in (select h from a278t b)) ss;
explain select * from (select a.u2 from a278t a where u1 in (select h from a278t b)) ss;
explain select u1 from a278t a where u1 in (select h from a278t b);
explain select * from (select max(u1) from a278t a where exists (select 1 from a278t b where b.th = a.u2)) ss;
create table a278c (t text, b char(10)) with (autovacuum_enabled = off);
create index a278c_t on a278c (t collate "C");
create unique index a278c_b on a278c (b collate "C" bpchar_pattern_ops);
explain select * from a278c where t = 'x';
explain select * from a278c where t < 'x';
explain select * from a278c where t collate "C" = 'x';
explain select * from a278c where t in ('x', 'y');
explain select * from a278c where b = 'h8';
drop table a278t, a278c;
