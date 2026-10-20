-- Equivalence classes in the join search (generate_join_implied_equalities): columns
-- equated through a third one can be joined directly, one equality per class at each
-- join, the class's columns carried up to where all its tables meet; a join nothing
-- above reads from is zero wide and returns no columns.
create table jeq1 (a int, t text);
create table jeq2 (b int, u text);
create table jeq3 (c int);
create table jeq4 (d int primary key, e int);
insert into jeq1 select case when g % 3 = 0 then 1 else g end, 't' || (g % 50) from generate_series(1, 2000) g;
insert into jeq2 select case when g % 2 = 0 then 1 else g % 100 end, 'T' || (g % 30) from generate_series(1, 3000) g;
insert into jeq3 select g % 5 from generate_series(1, 50) g;
insert into jeq4 select g, g % 10 from generate_series(1, 5000) g;
analyze jeq1; analyze jeq2; analyze jeq3; analyze jeq4;
explain select count(*) from jeq1 join jeq2 on a = b join jeq3 on c = b;
explain select count(*) from jeq1, jeq2, jeq3 where a = b and b = c;
explain select count(*) from jeq1, jeq2, jeq3 where a = b and a = c;
explain select * from jeq3, jeq1, jeq4 where c = a and a = d;
explain select * from jeq3 join jeq4 on c = d join jeq1 on a = d;
explain select count(*) from jeq1 join jeq2 on a = b join jeq3 on c = b join jeq4 on d = c;
select count(*) from jeq1 join jeq2 on a = b join jeq3 on c = b where a > 1;
explain select count(*) from jeq1 join jeq2 on a = b;
explain (verbose, costs off) select count(*) from jeq1 join jeq2 on a = b;
