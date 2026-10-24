-- name = text as a merge key: the class's collation is the name side's C, so the text
-- side sorts under it (Sort Key: ... COLLATE "C"), an outer join's sides too.
drop table if exists nk, tx;
create table nk(s name, v int);
create table tx(i text, w int);
insert into nk select 'A' || g, g from generate_series(1, 50) g;
insert into tx select 'A' || g, g from generate_series(3, 60) g;
analyze nk;
analyze tx;
set enable_hashjoin = off;
set enable_nestloop = off;
explain (costs off) select * from nk join tx on nk.s = tx.i;
explain (costs off, verbose) select * from nk join tx on tx.i = nk.s;
explain (costs off) select * from nk left join tx on nk.s = tx.i;
explain (costs off) select * from tx left join nk on nk.s = tx.i;
explain (costs off) select * from nk full join tx on nk.s = tx.i;
explain (costs off) select * from nk where s in (select i from tx);
explain (costs off) select * from nk join tx on nk.s = tx.i order by tx.i;
explain (costs off) select * from nk join tx on nk.s = tx.i order by nk.s;
select count(*), sum(v), sum(w) from nk join tx on nk.s = tx.i;
select s, v, i, w from nk full join tx on nk.s = tx.i order by v, w;
select s, i from tx left join nk on nk.s = tx.i order by w limit 5;
create index on tx (i);
set enable_sort = off;
explain (costs off) select * from nk join tx on nk.s = tx.i;
reset enable_sort;
reset enable_hashjoin;
reset enable_nestloop;
drop table nk, tx;
