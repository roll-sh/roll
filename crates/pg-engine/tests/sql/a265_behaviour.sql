-- An inner join's clause with a one-row FROM item of constants (an inlined SQL function's
-- `select $1`, a VALUES row) put in for its columns is its class's one source clause, given
-- back as written (generate_base_implied_equalities_const).
create table a265_t (a int);
insert into a265_t select generate_series(1, 100);
analyze a265_t;
create function a265_f(int) returns setof int language sql immutable as $$ select $1 $$;
explain (costs off) select * from a265_f(1) f, a265_t where f = a265_t.a;
explain (costs off) select * from a265_f(1) f, a265_t where a265_t.a = f;
explain (costs off) select * from a265_t, a265_f(1) f where f = a265_t.a;
explain (costs off) select * from (values (1)) v(x), a265_t where x = a265_t.a;
explain (costs off) select * from a265_f(1) f join a265_t on f = a265_t.a and a265_t.a > 0;
select * from a265_f(7) f, a265_t where f = a265_t.a;
-- A materialized view's rows have system columns, its storage's.
create materialized view a265_mv as select g as id from generate_series(1, 300) g;
select ctid, id from a265_mv where id in (1, 226, 227, 300) order by id;
select tableoid = 'a265_mv'::regclass as own, xmin = xmin as x from a265_mv limit 1;
refresh materialized view a265_mv;
select ctid, id from a265_mv where id in (1, 300) order by id;
select a265_mv.ctid, t.a from a265_mv join a265_t t on t.a = a265_mv.id where t.a < 3 order by 2;
drop materialized view a265_mv;
drop function a265_f(int);
drop table a265_t;
