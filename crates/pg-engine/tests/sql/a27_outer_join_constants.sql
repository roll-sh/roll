-- reconsider_outer_join_clauses: a left join's `outer = inner` whose outer column is equated
-- with a constant (in its own join domain or a lower one) gives the nullable side `inner =
-- constant`, and the clause is the constant-TRUE one wherever the outer relation isn't
-- null-extended yet. A constant class through an outer join's preserved side and a LATERAL
-- join: the subquery's output restricted (its Subquery Scan's filter), the join clause gone.
create table ojc_t (f1 text);
insert into ojc_t values ('doh!'), ('hi de ho neighbor');
vacuum ojc_t;
create table ojc_i (f1 int, g int);
insert into ojc_i values (0, 1), (1, 2), (1, 3), (2, 4), (null, 5);
analyze ojc_i;
explain (verbose, costs off)
select 1 from
  ojc_t as tt1
  inner join ojc_t as tt2 on (tt1.f1 = 'foo')
  left join ojc_t as tt3 on (tt3.f1 = 'foo')
  left join ojc_t as tt4 on (tt3.f1 = tt4.f1),
  lateral (select tt4.f1 as c0 from ojc_t as tt5 limit 1) as ss1
where tt1.f1 = ss1.c0;
select 1 from
  ojc_t as tt1
  inner join ojc_t as tt2 on (tt1.f1 = 'foo')
  left join ojc_t as tt3 on (tt3.f1 = 'foo')
  left join ojc_t as tt4 on (tt3.f1 = tt4.f1),
  lateral (select tt4.f1 as c0 from ojc_t as tt5 limit 1) as ss1
where tt1.f1 = ss1.c0;
explain (costs off)
select * from ojc_t tt1 left join ojc_t tt3 on (tt3.f1 = 'foo') left join ojc_t as tt4 on (tt3.f1 = tt4.f1);
explain (costs off)
select * from ojc_i t0 left join ojc_i tt3 on tt3.f1 = 1 and t0.g = tt3.g left join ojc_i tt4 on tt3.f1 = tt4.f1;
select * from ojc_i t0 left join ojc_i tt3 on tt3.f1 = 1 and t0.g = tt3.g left join ojc_i tt4 on tt3.f1 = tt4.f1
order by 1, 2, 3, 4, 5, 6;
explain (costs off)
select * from ojc_i t0 left join ojc_i tt3 on tt3.f1 = 1 left join ojc_i tt4 on tt3.f1 = tt4.f1;
select * from ojc_i t0 left join ojc_i tt3 on tt3.f1 = 1 left join ojc_i tt4 on tt3.f1 = tt4.f1
order by 1, 2, 3, 4, 5, 6;
explain (costs off)
select * from ojc_i t0 join ojc_i tt3 on tt3.f1 = 1 left join ojc_i tt4 on tt3.f1 = tt4.f1;
select * from ojc_i t0 join ojc_i tt3 on tt3.f1 = 1 left join ojc_i tt4 on tt3.f1 = tt4.f1
order by 1, 2, 3, 4, 5, 6;
explain (costs off)
select * from ojc_i tt4 right join (ojc_i t0 left join ojc_i tt3 on tt3.f1 = 1) on tt3.f1 = tt4.f1;
select * from ojc_i tt4 right join (ojc_i t0 left join ojc_i tt3 on tt3.f1 = 1) on tt3.f1 = tt4.f1
order by 1, 2, 3, 4, 5, 6;
explain (costs off)
select * from ojc_i t0 right join ojc_i tt3 on tt3.f1 = 1 and t0.g = tt3.g right join ojc_i tt4 on tt3.f1 = tt4.f1;
select * from ojc_i t0 right join ojc_i tt3 on tt3.f1 = 1 and t0.g = tt3.g right join ojc_i tt4 on tt3.f1 = tt4.f1
order by 1, 2, 3, 4, 5, 6;
drop table ojc_t;
drop table ojc_i;
