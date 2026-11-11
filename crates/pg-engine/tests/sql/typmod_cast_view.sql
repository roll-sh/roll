create table gx_tm_numeric_tbl (f1 numeric(18,3), f2 numeric, v varchar(10), c char(3));
create view gx_tm_numeric_view as select f1, f1::numeric(16,4) as f1164, f1::numeric as f1n, f2, f2::numeric(16,4) as f2164, f2::numeric as f2n, v::varchar as vv, c::bpchar as cc from gx_tm_numeric_tbl;
select pg_get_viewdef('gx_tm_numeric_view'::regclass);
select attname, format_type(atttypid, atttypmod) from pg_attribute where attrelid = 'gx_tm_numeric_view'::regclass and attnum > 0 order by attnum;
explain (verbose, costs off) select * from gx_tm_numeric_view;
select pg_typeof((1.5::numeric(5,2))::numeric), 1.5::numeric(5,2)::numeric;
drop view gx_tm_numeric_view; drop table gx_tm_numeric_tbl;
