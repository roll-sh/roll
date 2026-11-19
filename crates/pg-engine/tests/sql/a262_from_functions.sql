-- A PL function called in FROM is one relation, its Function Scan: the join search orders it
-- like a table (it is never materialized: it keeps its rows for a rescan), its quals and
-- joins name it by its alias.
create table ff_t1 (a int);
insert into ff_t1 select generate_series(1, 100);
analyze ff_t1;
create function ff_set(int) returns setof int language plpgsql immutable as $$ begin return next $1; end $$;
explain (costs off) select * from ff_set(1) f where f > 0;
explain (costs off) select * from ff_set(1) as f(x) where x > 0;
explain (costs off, verbose) select * from ff_set(1) f where f > 0;
explain (costs off) select * from ff_set(1) f join ff_t1 on f = ff_t1.a;
explain (costs off, verbose) select * from ff_set(1) f, ff_t1 where f = ff_t1.a;
select * from ff_set(5) f join ff_t1 on f = ff_t1.a;
-- Over an empty table the function runs first, as PostgreSQL's plan has it: its error.
create table ff_t0 (a int);
create function ff_div(int) returns setof int language plpgsql immutable as $$ begin raise notice 'ff_div %', $1; return next 10 / $1; end $$;
explain select * from ff_t0, ff_div(0);
explain select * from ff_div(0), ff_t0;
select * from ff_t0, ff_div(0);
drop function ff_set(int), ff_div(int);
drop table ff_t0, ff_t1;
