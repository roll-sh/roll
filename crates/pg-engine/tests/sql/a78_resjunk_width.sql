-- Round 21, agent 78: a Sort or Limit under a projection only dropping the ORDER BY's
-- resjunk columns returns them: its width counts them (agent 70's finding).
create table a78_rj (id int, v text);
insert into a78_rj select g, 'x' || g from generate_series(1, 1000) g;
analyze a78_rj;
explain select id from a78_rj order by id * 2.5 limit 5;
explain select id from a78_rj order by id * 2.5;
explain select id from a78_rj order by v;
explain select id from a78_rj order by v limit 3;
explain select id from a78_rj order by length(v), id;
explain select id, count(*) from a78_rj group by id order by sum(id) * 2;
explain select distinct id from a78_rj order by id;
explain (verbose) select id from a78_rj order by id * 2.5 limit 5;
select id from a78_rj order by id * 2.5 desc limit 3;
drop table a78_rj;
