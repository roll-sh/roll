-- agent 285: a FROM call folding to a constant is an RTE_RESULT (pull_up_constant_function),
-- read as it is or under a projection with calls that don't fold (a volatile one stays a call
-- over the constant); a scan's system columns are as wide as their types when read
-- (tid 6; xid, cid, oid 4).
create function a285pair(anycompatible, anycompatible) returns anycompatiblearray as 'select array[$1, $2]' language sql;
explain select x, pg_typeof(x) from a285pair(11, 12) x;
select x, pg_typeof(x) from a285pair(11, 12) x;
explain select * from to_tsquery('english', 'moscow') as query;
explain select * from upper('a') as u;
select * from upper('a') as u;
explain select * from abs(-3) as u, generate_series(1, 2) g;
select * from abs(-3) as u, generate_series(1, 2) g;
explain select random() > 2 from abs(-3) as u;
explain select u + 1 from abs(-3) as u where u > 0;
select u + 1 from abs(-3) as u where u > 0;
explain select * from abs(-3) as u left join generate_series(1, 5) g on g = u;
create table a285t (id int, v text) with (autovacuum_enabled = off);
insert into a285t select g, g::text from generate_series(1, 20) g;
explain select t1.ctid, t1.*, t2.ctid, t2.* from a285t t1, a285t t2 where t1.id = 1 and t2.ctid = t1.ctid;
select t1.ctid, t1.*, t2.ctid from a285t t1, a285t t2 where t1.id = 1 and t2.ctid = t1.ctid;
explain select ctid, xmin, cmin, tableoid, id from a285t where id = 2;
drop function a285pair(anycompatible, anycompatible);
drop table a285t;
