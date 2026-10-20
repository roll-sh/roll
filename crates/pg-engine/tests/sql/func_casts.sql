-- typename(arg) read as a cast (func_get_detail's FUNCDETAIL_COERCION).
select int4('x');
select date('x');
select inet('zz');
select text(1);
select int4('12'), text(true), varchar(12.5), bool('t'), xml('<a/>'), xml('<a');
select int8(int2 '3'), float8(1), date(timestamp '2020-01-02 03:04');
create table gx_f(a int, b text);
create view gx_fv as select text(a) t, int4(b) i, bpchar(a) c from gx_f;
select pg_get_viewdef('gx_fv');
select int4(1.5::numeric), text(row(1,2));
select timestamptz('2020-01-01'), interval('1 day');
drop view gx_fv; drop table gx_f;
