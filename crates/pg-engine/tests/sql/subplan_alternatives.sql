-- A correlated EXISTS kept as a subplan gets a hashed alternative (convert_EXISTS_to_ANY),
-- the cheaper for how often it runs kept (fix_alternative_subplan); sub-select costs in
-- quals and target lists (cost_subplan), InitPlans charged to their level; correlated
-- IN pulled up into a semi join with its outer references as join clauses.
create table sap_1 (a int, b int, t text);
create table sap_2 (c int, d int);
create table sap_3 (e int);
insert into sap_1 select g, g % 10, 't' || g from generate_series(1, 1000) g;
insert into sap_2 select g % 300, g from generate_series(1, 5000) g;
insert into sap_3 values (1), (2), (3);
analyze sap_1; analyze sap_2; analyze sap_3;
explain select * from sap_1 where exists (select 1 from sap_2 where c = a) or b = 1;
explain select * from sap_1 where a in (select c from sap_2) or b = 1;
explain select * from sap_1 where not exists (select 1 from sap_2 where c = a) or b = 1;
explain select * from sap_1 where a not in (select c from sap_2);
explain select * from sap_1 where a = (select max(c) from sap_2);
explain select *, (select count(*) from sap_2) from sap_1 where b < 3;
explain select * from sap_1 where exists (select 1 from sap_2 where c = a and d > b) or b = 1;
explain select a, exists (select 1 from sap_2 where c = a) from sap_1;
explain select * from sap_1 where (exists (select 1 from sap_2 where c = a) or b = 1) and a in (select d from sap_2 where d < 100 or a = 3);
explain select * from sap_1 where exists (select 1 from sap_2 where c = a and d > 4990) or b = 1;
explain select * from sap_1 where b = 1 and (exists (select 1 from sap_2 where c = a) or a > 990);
explain select e, exists (select 1 from sap_2 where c = e) from sap_3;
explain select * from sap_3 where exists (select 1 from sap_2 where c = e) or e = 1;
select count(*) from sap_1 where exists (select 1 from sap_2 where c = a) or b = 1;
select count(*) from sap_1 where not exists (select 1 from sap_2 where c = a) or b = 1;
