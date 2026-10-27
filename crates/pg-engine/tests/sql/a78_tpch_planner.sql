-- Round 21, agent 78: TPC-H plan causes, on small tables.
create table a78_n as select g as k, 'N' || g as name, g % 5 as r from generate_series(0, 24) g;
create table a78_s as select g as k, g % 25 as nk, 'S' || g as name from generate_series(1, 1000) g;
create table a78_c as select g as k, g % 25 as nk from generate_series(1, 5000) g;
create table a78_o as select g as k, g % 5000 + 1 as ck, (g % 50)::numeric as qty from generate_series(1, 20000) g;
create table a78_l as select g as k, g % 20000 + 1 as ok, g % 1000 + 1 as sk, (g % 47)::numeric as qty from generate_series(1, 60000) g;
alter table a78_n add primary key (k);
alter table a78_s add primary key (k);
alter table a78_c add primary key (k);
alter table a78_o add primary key (k);
create index on a78_l (ok);
create index on a78_l (sk, ok);
analyze a78_n, a78_s, a78_c, a78_o, a78_l;

-- A join OR clause's restrictions taken out for each relation (extract_restriction_or_clauses)
-- make up for themselves in the clause's selectivity, the relations of a FROM subquery
-- pulled up too (TPC-H q07).
explain select * from (select n1.name as a, n2.name as b from a78_c c, a78_n n1, a78_n n2
  where c.nk = n2.k
    and ((n1.name = 'N1' and n2.name = 'N2') or (n1.name = 'N2' and n2.name = 'N1'))) x;
select count(*) from (select n1.name as a, n2.name as b from a78_c c, a78_n n1, a78_n n2
  where c.nk = n2.k
    and ((n1.name = 'N1' and n2.name = 'N2') or (n1.name = 'N2' and n2.name = 'N1'))) x;

-- An IN over a sub-select grouped by the compared column, among other joins: an inner join
-- (reduce_unique_semijoins; TPC-H q18).
explain (costs off) select c.k, o.k, sum(l.qty) from a78_c c, a78_o o, a78_l l
  where o.k in (select ok from a78_l group by ok having sum(qty) > 80)
    and c.k = o.ck and o.k = l.ok
  group by c.k, o.k order by 3 desc, 1, 2 limit 5;
select c.k, o.k, sum(l.qty) from a78_c c, a78_o o, a78_l l
  where o.k in (select ok from a78_l group by ok having sum(qty) > 80)
    and c.k = o.ck and o.k = l.ok
  group by c.k, o.k order by 3 desc, 1, 2 limit 5;

-- An IN pulled up over a sub-select whose WHERE has a sub-select of its own reading only its
-- rows (TPC-H q20).
explain (costs off) select s.name from a78_s s, a78_n n
  where s.k in (select l.sk from a78_l l where l.ok in (select k from a78_o where qty < 10)
                and l.qty > (select 0.5 * sum(l2.qty) from a78_l l2 where l2.sk = l.sk and l2.ok = l.ok))
    and s.nk = n.k and n.name = 'N3'
  order by s.name;
select s.name from a78_s s, a78_n n
  where s.k in (select l.sk from a78_l l where l.ok in (select k from a78_o where qty < 10)
                and l.qty > (select 0.5 * sum(l2.qty) from a78_l l2 where l2.sk = l.sk and l2.ok = l.ok))
    and s.nk = n.k and n.name = 'N3'
  order by s.name;

-- DISTINCT under an ORDER BY of its columns, descending: its keys take the ORDER BY's
-- directions, one sort for both.
explain select distinct ck, qty from a78_o order by 1 desc;
explain select distinct ck from a78_o order by 1 desc;
explain select distinct ck, k from a78_o where k < 1000 order by 1 desc;
select distinct ck, k from a78_o where k < 1000 and ck > 990 order by 1 desc, 2;
select distinct ck, qty from a78_o where ck < 4 order by 1 desc, 2;

-- A grouping whose aggregate has no partial form (count(DISTINCT)): the join's partial plans
-- sorted in the workers for it (the group keys, then the aggregate's argument) under a
-- Gather Merge (TPC-H q16).
set parallel_setup_cost = 0;
set parallel_tuple_cost = 0;
set min_parallel_table_scan_size = 0;
explain (costs off) select o.ck, count(distinct l.sk) from a78_o o join a78_l l on l.ok = o.k
  where o.qty < 25 group by o.ck;
select o.ck, count(distinct l.sk) from a78_o o join a78_l l on l.ok = o.k
  where o.qty < 25 and o.ck < 4 group by o.ck order by 1;
reset parallel_setup_cost;
reset parallel_tuple_cost;
reset min_parallel_table_scan_size;

drop table a78_n, a78_s, a78_c, a78_o, a78_l;
