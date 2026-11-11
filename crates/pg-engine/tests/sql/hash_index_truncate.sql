-- TRUNCATE gives a table's indexes new files built over the empty heap: a hash index
-- starts again with the buckets estimate_rel_size's guess for empty files gives.
create table hit (a int, b text);
insert into hit select g, 'x' || g from generate_series(1, 20000) g;
create index hit_a on hit using hash (a);
select pg_relation_size('hit_a');
analyze hit;
select relpages, reltuples from pg_class where relname = 'hit';
truncate hit;
select relname, relpages, reltuples from pg_class where relname in ('hit', 'hit_a') order by relname;
select pg_relation_size('hit_a');
insert into hit select g, 'x' || g from generate_series(1, 5000) g;
select pg_relation_size('hit_a'), pg_relation_size('hit');
begin;
truncate hit;
insert into hit select g, 'y' from generate_series(1, 100) g;
select pg_relation_size('hit_a');
commit;
