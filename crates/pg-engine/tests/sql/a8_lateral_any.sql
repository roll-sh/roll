-- convert_ANY_sublink_to_join over a correlated aggregate sub-select (subselect 327's shape):
-- a LATERAL subquery ("ANY_subquery") the join reads, an inner join when it returns one row
-- at most (reduce_unique_semijoins), a semi join for a grouped one; the MIN / MAX InitPlan
-- inside reads the LATERAL values.
create temp table la_b (unique1 int, hundred int, odd int);
insert into la_b select g, g % 100, (g % 100) * 2 + 1 from generate_series(0, 9999) g;
create index la_b_hundred on la_b (hundred);
create temp table la_a (unique1 int, hundred int);
insert into la_a select g, g % 100 from generate_series(0, 999) g;
analyze la_b;
analyze la_a;

explain (costs off)
select * from la_a a left join la_b b
on b.hundred in (select min(c.hundred) from la_b c where c.odd = b.odd);

select count(*), count(b.unique1), sum(b.unique1)
from (select * from la_a where unique1 < 3) a
  left join (select * from la_b where unique1 % 7 = 0) b
  on b.hundred in (select min(c.hundred) from la_b c where c.odd = b.odd and c.unique1 > 500);

select a.unique1, b.unique1
from (select * from la_a where unique1 < 3) a
  left join (select * from la_b where unique1 < 40) b
  on b.hundred in (select max(c.hundred) from la_b c where c.odd = b.odd and c.unique1 > 5)
order by 1, 2;

explain (costs off)
select b.unique1 from la_b b
where b.unique1 < 20 and b.hundred in (select min(c.hundred) from la_b c where c.odd = b.odd);

select b.unique1, b.hundred from la_b b
where b.unique1 < 50 and b.hundred in (select max(c.hundred) from la_b c where c.odd = b.odd and c.unique1 < 30)
order by 1;

select b.unique1, b.hundred from la_b b
where b.unique1 < 50 and b.hundred in (select count(*) from la_b c where c.odd = b.odd and c.unique1 < 30)
order by 1;

explain (costs off)
select b.unique1, b.hundred from la_b b
where b.unique1 < 300
  and b.hundred in (select sum(c.unique1) / 100 from la_b c where c.odd = b.odd and c.unique1 < 300 group by c.unique1 % 3);

select b.unique1, b.hundred from la_b b
where b.unique1 < 300
  and b.hundred in (select sum(c.unique1) / 100 from la_b c where c.odd = b.odd and c.unique1 < 300 group by c.unique1 % 3)
order by 1;

select b.unique1 from la_b b
where b.unique1 < 20 and (b.hundred, b.odd) in (select min(c.hundred), min(c.odd) from la_b c where c.odd = b.odd)
order by 1;

select b.unique1 from la_b b
where b.unique1 < 20 and b.hundred in (select min(c.hundred) from la_b c where c.odd = b.odd having count(*) > 1000)
order by 1;
