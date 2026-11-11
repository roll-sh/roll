create table aico_t (id int, b int);
create index aico_t_i on aico_t (id);
alter index aico_t_i alter column id set (n_distinct=100);
alter index aico_t_i alter column 1 set (n_distinct=100);
alter index aico_t_i alter column id reset (n_distinct);
create table aico_p (id int) partition by range (id);
create index aico_p_i on aico_p (id);
alter index aico_p_i alter column id set (n_distinct=100);
alter index aico_t_i alter column id set statistics 100;
alter index aico_t_i alter column 1 set statistics 100;
-- min/max over anyarray values of different element types
select max(histogram_bounds) is not null from pg_stats where tablename = 'pg_am';
drop table aico_t, aico_p;
