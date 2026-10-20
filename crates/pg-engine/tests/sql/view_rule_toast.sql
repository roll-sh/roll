-- pg_rewrite.ev_action is the rule's query tree; a large one is stored in pg_rewrite's
-- TOAST table, which takes an oid for the value (after the rule's), so later objects'
-- oids move on by one.
create table vrt_t (x int primary key, y text, z numeric);
create view vrt_v0 as select x, y from vrt_t where z > 0;
create table vrt_n0 (q int);
create view vrt_v1 as select x, y, z, x+1 as a1, x+2 as a2, x+3 as a3, x+4 as a4, x+5 as a5, x+6 as a6, x+7 as a7, x+8 as a8, x+9 as a9, x+10 as a10, x+11 as a11 from vrt_t;
create table vrt_n1 (q int);
create view vrt_v2 as select x, y, z, x+1 as a1, x+2 as a2, x+3 as a3, x+4 as a4, x+5 as a5, x+6 as a6, x+7 as a7, x+8 as a8, x+9 as a9, x+10 as a10, x+11 as a11, upper(y) u1, lower(y) u2, length(y) u3, z*2 m1, z/3 m2, coalesce(y, 'q') c1, case when x > 1 then 'a' when x > 2 then 'b' else 'c' end k1, x::text t1, y || 'zz' t2, now() n1, current_date d1, abs(x) b1, sign(z) s1, round(z) r1, trunc(z, 2) r2 from vrt_t where x > 0 and y like 'a%' and z between 1 and 10;
create table vrt_n2 (q int);
create or replace view vrt_v2 as select x, y, z, x+1 as a1, x+2 as a2, x+3 as a3, x+4 as a4, x+5 as a5, x+6 as a6, x+7 as a7, x+8 as a8, x+9 as a9, x+10 as a10, x+11 as a11, upper(y) u1, lower(y) u2, length(y) u3, z*2 m1, z/3 m2, coalesce(y, 'q') c1, case when x > 1 then 'a' when x > 2 then 'b' else 'c' end k1, x::text t1, y || 'zz' t2, now() n1, current_date d1, abs(x) b1, sign(z) s1, round(z) r1, trunc(z, 2) r2, 1 as e1 from vrt_t where x > 0 and y like 'a%' and z between 1 and 10;
create table vrt_n3 (q int);
create view vrt_v3 (p, q) as select t.x, count(*) from vrt_t t join vrt_t u using (x) group by t.x having count(*) > 1 order by 2;
create materialized view vrt_m1 as select x, y, z, x+1 as a1, x+2 as a2, x+3 as a3, x+4 as a4, x+5 as a5, x+6 as a6, x+7 as a7, x+8 as a8, x+9 as a9, x+10 as a10, x+11 as a11, upper(y) u1, lower(y) u2, length(y) u3, z*2 m1, z/3 m2, coalesce(y, 'q') c1, case when x > 1 then 'a' when x > 2 then 'b' else 'c' end k1, x::text t1, y || 'zz' t2, abs(x) b1, sign(z) s1, round(z) r1, trunc(z, 2) r2, x+12 a12, x+13 a13, x+14 a14 from vrt_t where x > 0 and y like 'a%' and z between 1 and 10;
create table vrt_n4 (q int);
select relname, oid::int8 - 'vrt_t'::regclass::oid::int8 as delta from pg_class where relname like 'vrt\_%' and relkind in ('r', 'v', 'm') order by oid;
select r.ev_class::regclass, length(ev_action), md5(regexp_replace(ev_action::text, ':(relid|resorigtbl) [1-9][0-9]{4,}', ':\1 X', 'g')) from pg_rewrite r where ev_class::regclass::text like 'vrt\_%' order by 1;
select regexp_replace(ev_action::text, ':(relid|resorigtbl) [1-9][0-9]{4,}', ':\1 X', 'g') from pg_rewrite where ev_class = 'vrt_v0'::regclass;
drop table vrt_t cascade;
drop table vrt_n0, vrt_n1, vrt_n2, vrt_n3, vrt_n4;
