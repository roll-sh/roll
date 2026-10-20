-- Join size estimates use foreign keys (get_foreign_key_join_selectivity): a join whose
-- clauses equate every column of a foreign key gets 1 / the referenced table's tuples.
create table fkj_1 (id int primary key, k int, v text);
create table fkj_2 (id int, f1 int references fkj_1, w text);
create table fkj_3 (a int, b int, primary key (a, b));
insert into fkj_1 select g, g % 50, 'v' || g from generate_series(1, 2000) g;
insert into fkj_2 select g, (g % 2000) + 1, 'w' from generate_series(1, 9000) g;
insert into fkj_3 select g % 100, g / 100 from generate_series(0, 2999) g;
alter table fkj_2 add column fa int, add column fb int;
update fkj_2 set fa = id % 100, fb = (id % 3000) / 100;
alter table fkj_2 add foreign key (fa, fb) references fkj_3;
analyze fkj_1; analyze fkj_2; analyze fkj_3;
explain select * from fkj_2 join fkj_1 on fkj_1.id = fkj_2.f1;
explain select * from fkj_2 join fkj_1 on fkj_1.id = fkj_2.f1 where fkj_1.k = 3;
explain select * from fkj_2 left join fkj_1 on fkj_1.id = fkj_2.f1;
explain select * from fkj_2 where exists (select 1 from fkj_1 where fkj_1.id = fkj_2.f1);
explain select * from fkj_2 where not exists (select 1 from fkj_1 where fkj_1.id = fkj_2.f1);
explain select * from fkj_2 join fkj_3 on a = fa and b = fb;
explain select * from fkj_2 join fkj_3 on a = fa;
explain select * from fkj_1 join fkj_2 on fkj_1.id = fkj_2.f1 join fkj_3 on a = fa and b = fb;
alter table fkj_2 alter constraint fkj_2_f1_fkey not enforced;
explain select * from fkj_2 join fkj_1 on fkj_1.id = fkj_2.f1 where fkj_1.k = 3;
