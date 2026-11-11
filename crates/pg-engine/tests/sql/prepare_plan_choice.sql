-- After five custom plans a prepared statement weighs the generic plan's cost against the
-- custom plans' average (planning charged): it keeps planning custom ones while they're
-- cheaper (choose_custom_plan)
create table sk (a int, b text);
insert into sk select case when g <= 990 then 1 else g end, 'v' || g from generate_series(1, 1000) g;
create index sk_a on sk (a);
analyze sk;
prepare q(int) as select b from sk where a = $1;
execute q(995); execute q(996); execute q(997); execute q(998); execute q(999);
explain execute q(1000);
execute q(1000);
select generic_plans, custom_plans from pg_prepared_statements where name = 'q';
explain execute q(1);
select generic_plans, custom_plans from pg_prepared_statements where name = 'q';
-- the generic plan wins when it costs no more
prepare g(int) as select b from sk where a = $1 + 0 and b = 'x';
execute g(1); execute g(1); execute g(1); execute g(1); execute g(1);
explain execute g(1);
select generic_plans, custom_plans from pg_prepared_statements where name = 'g';
-- a query without relations: custom plans pay planning, the generic one wins
prepare r(int) as select $1 + 1;
execute r(1); execute r(1); execute r(1); execute r(1); execute r(1); execute r(1);
select generic_plans, custom_plans from pg_prepared_statements where name = 'r';
-- a join: planning is charged per range table entry
create table sk2 (a int primary key, c text);
insert into sk2 select g, 'c' || g from generate_series(1, 1000) g;
analyze sk2;
prepare j(int) as select sk.b, sk2.c from sk join sk2 on sk2.a = sk.a where sk.a = $1;
execute j(995); execute j(996); execute j(997); execute j(998); execute j(999);
explain execute j(1000);
select generic_plans, custom_plans from pg_prepared_statements where name = 'j';
prepare j2(int) as select sk.b, sk2.c from sk, sk2 where sk2.a = sk.a and sk.a = $1;
execute j2(995); execute j2(996); execute j2(997); execute j2(998); execute j2(999);
explain execute j2(1000);
select generic_plans, custom_plans from pg_prepared_statements where name = 'j2';
-- after the generic plan is known its cost decides each time
prepare k(int) as select count(b) from sk where a = $1;
execute k(1); execute k(1); execute k(1); execute k(1); execute k(1);
execute k(1);
execute k(995);
select generic_plans, custom_plans from pg_prepared_statements where name = 'k';
-- a range whose custom plans are cheap index scans: the generic plan (a third of the rows)
-- costs more, so custom plans go on being made
prepare lt(int) as select c from sk2 where a < $1;
execute lt(2); execute lt(3); execute lt(2); execute lt(3); execute lt(2);
explain execute lt(4);
execute lt(4);
execute lt(5);
select generic_plans, custom_plans from pg_prepared_statements where name = 'lt';
-- plan_cache_mode overrides the costs; the counts go on
set plan_cache_mode = force_generic_plan;
execute lt(4);
reset plan_cache_mode;
explain execute lt(4);
select generic_plans, custom_plans from pg_prepared_statements where name = 'lt';
-- the generic plan is costed anew when it is rebuilt
create index sk2_c on sk2 (c);
explain execute lt(4);
select generic_plans, custom_plans from pg_prepared_statements where name = 'lt';
-- DML
prepare u(int) as update sk set b = b where a = $1;
execute u(995); execute u(996); execute u(997); execute u(998); execute u(999);
execute u(1000);
select generic_plans, custom_plans from pg_prepared_statements where name = 'u';
deallocate all;
drop table sk, sk2;
