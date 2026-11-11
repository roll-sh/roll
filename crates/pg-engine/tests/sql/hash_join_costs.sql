-- Hash join costs: bucket sizes and MCV frequencies of the inner keys
-- (estimate_hash_bucket_stats) through varchar relabels, I/O-conversion keys costed as two
-- functions, numeric and float keys hashed.
create table hjc_1 (a int, t text, v varchar(20));
create table hjc_2 (b int, u text, w varchar(20));
create table hjc_3 (c int);
insert into hjc_1 select case when g % 3 = 0 then 1 else g end, 't' || (g % 50), 'v' || (g % 9) from generate_series(1, 2000) g;
insert into hjc_2 select case when g % 2 = 0 then 1 else g % 100 end, 'T' || (g % 30), 'v' || (g % 4) from generate_series(1, 3000) g;
insert into hjc_3 select g % 5 from generate_series(1, 50) g;
analyze hjc_1; analyze hjc_2; analyze hjc_3;
explain select * from hjc_1 join hjc_2 on a = b;
explain select * from hjc_2 join hjc_1 on a = b;
explain select * from hjc_1 join hjc_2 on v = w;
explain select * from hjc_1 join hjc_3 on a = c;
explain select * from hjc_3 join hjc_1 on a = c;
explain select * from hjc_1 join hjc_2 on a = b and t = u;
explain select * from hjc_1 left join hjc_2 on a = b;
explain select * from hjc_1 full join hjc_2 on a = b;
explain select * from hjc_1 where a in (select b from hjc_2);
explain select * from hjc_1 where a not in (select b from hjc_2 where b is not null);
explain select * from hjc_1 where not exists (select 1 from hjc_2 where b = a);
create table hjc_4 (a int, t text, n numeric);
create table hjc_5 (b int, u text, m numeric);
insert into hjc_4 select g, 't' || (g % 50), g % 13 from generate_series(1, 200) g;
insert into hjc_5 select g % 40, 'T' || (g % 30), g % 7 from generate_series(1, 300) g;
analyze hjc_4; analyze hjc_5;
explain select * from hjc_4 join hjc_5 on n = m;
explain select * from hjc_4 join hjc_5 on a = b and n = m;
explain select * from hjc_4 join hjc_5 on upper(t) = u;
set enable_mergejoin = off;
explain select * from hjc_4 join hjc_5 on a + 1 = b + 1;
explain select * from hjc_4 join hjc_5 on length(t) = b;
explain select * from hjc_4 join hjc_5 on a::text = u;
explain select * from hjc_4 join hjc_5 on (a % 10) = (b % 10);
reset enable_mergejoin;
select count(*), sum(a) from hjc_4 join hjc_5 on n = m;
