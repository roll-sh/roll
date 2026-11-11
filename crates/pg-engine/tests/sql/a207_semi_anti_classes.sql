-- Agent 207: TPC-H q21's join order, on small tables (every row in ANALYZE's sample).
-- A semi join's equality is in the equivalence class of the columns it equates though an
-- anti join is beside it: the join to orders below the semi join gets the class's clause
-- with orders' member (o.ok = l2.ok) as a join filter, so the semi join goes first.
-- And a semi join's inner index scan that checks a join clause as a filter (l2.sk <> l1.sk)
-- costs as a plain rescan (has_indexed_join_quals).
create table t21_n (nk int primary key, name text);
create table t21_s (sk int primary key, nk int, name text);
create table t21_o (ok int primary key, st char(1));
create table t21_l (ok int, ln int, sk int, cd int, rd int, primary key (ok, ln));
insert into t21_n select g, 'N' || g from generate_series(0, 24) g;
insert into t21_s select g, g % 25, 'S' || g from generate_series(1, 1000) g;
insert into t21_o select g, case when g % 2 = 0 then 'F' else 'O' end from generate_series(1, 7000) g;
insert into t21_l select o, ln, (o * 7 + ln * 13) % 1000 + 1, (o + ln) % 30, (o * 3 + ln) % 31 from generate_series(1, 7000) o, generate_series(1, 4) ln;
analyze t21_n, t21_s, t21_o, t21_l;
set max_parallel_workers_per_gather = 0;
explain select s.name, count(*) from t21_s s, t21_l l1, t21_o o, t21_n n
  where s.sk = l1.sk and o.ok = l1.ok and o.st = 'F' and l1.rd > l1.cd
    and exists (select * from t21_l l2 where l2.ok = l1.ok and l2.sk <> l1.sk)
    and not exists (select * from t21_l l3 where l3.ok = l1.ok and l3.sk <> l1.sk and l3.rd > l3.cd)
    and s.nk = n.nk and n.name = 'N1'
  group by s.name order by 2 desc, 1 limit 100;
select s.name, count(*) from t21_s s, t21_l l1, t21_o o, t21_n n
  where s.sk = l1.sk and o.ok = l1.ok and o.st = 'F' and l1.rd > l1.cd
    and exists (select * from t21_l l2 where l2.ok = l1.ok and l2.sk <> l1.sk)
    and not exists (select * from t21_l l3 where l3.ok = l1.ok and l3.sk <> l1.sk and l3.rd > l3.cd)
    and s.nk = n.nk and n.name = 'N1'
  group by s.name order by 2 desc, 1 limit 5;
-- Without the class (o.ok = l1.ok + 0), the join to orders goes first: a tie, the first
-- path made kept.
explain select s.name, count(*) from t21_s s, t21_l l1, t21_o o, t21_n n
  where s.sk = l1.sk and o.ok = l1.ok + 0 and o.st = 'F' and l1.rd > l1.cd
    and exists (select * from t21_l l2 where l2.ok = l1.ok and l2.sk <> l1.sk)
    and not exists (select * from t21_l l3 where l3.ok = l1.ok and l3.sk <> l1.sk and l3.rd > l3.cd)
    and s.nk = n.nk and n.name = 'N1'
  group by s.name order by 2 desc, 1 limit 100;
-- A constant through the semi join's class, beside an anti join.
select * from t21_o o
  where o.ok = 5 and exists (select 1 from t21_l l where l.ok = o.ok)
    and not exists (select 1 from t21_l l3 where l3.ok = o.ok and l3.sk = 1);
explain (costs off) select count(*) from t21_o o, t21_l l1
  where o.ok = l1.ok and exists (select 1 from t21_l l2 where l2.ok = l1.ok and l2.sk <> l1.sk)
    and not exists (select 1 from t21_l l3 where l3.ok = o.ok and l3.rd > 29);
select count(*) from t21_o o, t21_l l1
  where o.ok = l1.ok and exists (select 1 from t21_l l2 where l2.ok = l1.ok and l2.sk <> l1.sk)
    and not exists (select 1 from t21_l l3 where l3.ok = o.ok and l3.rd > 29);
-- In parallel (setup and tuple costs free): the anti join's Gather path, which won at its
-- relation, under the serial semi join and join to orders (a tie with the Gather above
-- them: the first path made kept).
set max_parallel_workers_per_gather = 2;
set parallel_setup_cost = 0;
set parallel_tuple_cost = 0;
set min_parallel_table_scan_size = 0;
set min_parallel_index_scan_size = 0;
explain select s.name, count(*) from t21_s s, t21_l l1, t21_o o, t21_n n
  where s.sk = l1.sk and o.ok = l1.ok and o.st = 'F' and l1.rd > l1.cd
    and exists (select * from t21_l l2 where l2.ok = l1.ok and l2.sk <> l1.sk)
    and not exists (select * from t21_l l3 where l3.ok = l1.ok and l3.sk <> l1.sk and l3.rd > l3.cd)
    and s.nk = n.nk and n.name = 'N1'
  group by s.name order by 2 desc, 1 limit 100;
