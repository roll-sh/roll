-- A join proven empty: its Result shows the target list over the removed relations' columns,
-- named by their relations, system catalogs' too (join 368's shape, without the
-- information_schema view).
create temp table dn_i4 (f1 int);
create temp table dn_i8 (q1 bigint, q2 bigint);

explain (verbose, costs off)
select c.relname, t2.q1 from dn_i8 t2 left join pg_class c on null where false;

explain (verbose, costs off)
select v.n, t2.q1 from dn_i8 t2 left join (select relname::text || 'x' as n from pg_class) v on null where false;

explain (verbose, costs off)
select v.n, t2.q1
from dn_i4 t1 inner join (dn_i8 t2 left join (select relname::text || 'x' as n from pg_class) v on null) on null;

select v.n, t2.q1
from dn_i4 t1 inner join (dn_i8 t2 left join (select relname::text || 'x' as n from pg_class) v on null) on null;
