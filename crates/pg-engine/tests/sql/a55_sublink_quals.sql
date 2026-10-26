-- convert_ANY_sublink_to_join / convert_EXISTS_sublink_to_join pull up a sub-select whose own
-- WHERE has sub-selects (a NOT IN's hashed SubPlan, a correlated scalar sub-select): they go
-- along as the semi join's side's quals.
create table a55_t (a int, b int);
create table a55_u (x int, y int);
create table a55_v (z int);
insert into a55_t select g, g from generate_series(1, 100) g;
insert into a55_t values (null, 1);
insert into a55_u select g, g % 10 from generate_series(1, 50) g;
insert into a55_u values (null, 3), (7, null);
insert into a55_v values (1), (2), (null);
analyze a55_t, a55_u, a55_v;
explain (costs off) select * from a55_t where a in (select x from a55_u where y > (select min(z) from a55_v where z < x));
select count(*) from a55_t where a in (select x from a55_u where y > (select min(z) from a55_v where z < x));
explain (costs off) select * from a55_t where a in (select x from a55_u where y not in (select z from a55_v where z is not null));
select count(*) from a55_t where a in (select x from a55_u where y not in (select z from a55_v where z is not null));
explain (costs off) select * from a55_t where a not in (select x from a55_u where y not in (select z from a55_v where z is not null));
explain (costs off) select * from a55_t where exists (select 1 from a55_u where x = a and y not in (select z from a55_v where z is not null));
select count(*) from a55_t where exists (select 1 from a55_u where x = a and y not in (select z from a55_v where z is not null));
explain (costs off) select * from a55_t where a in (select x from a55_u where y in (select z from a55_v) or y = 5);
select count(*) from a55_t where a in (select x from a55_u where y in (select z from a55_v) or y = 5);
drop table a55_t, a55_u, a55_v;
