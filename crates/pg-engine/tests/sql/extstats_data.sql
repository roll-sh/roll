-- pg_statistic_ext_data, pg_stats_ext and pg_stats_ext_exprs after ANALYZE: ndistinct,
-- functional dependencies, MCV lists (serialized as Postgres does) and expression statistics.
create table es (a int, b int, c text, d numeric, e bool);
insert into es select i % 10, i % 5, 'v' || (i % 7), (i % 3) * 1.5, i % 2 = 0 from generate_series(1, 1000) i;
insert into es values (null, null, null, null, null), (1, null, 'x', 2.25, null);
create statistics es1 on a, b, c from es;
create statistics es2 (mcv) on a, d, e from es;
create statistics es3 (ndistinct, dependencies) on b, (a + b), lower(c) from es;
create statistics es4 on (a * 2) from es;
select stxname, stxkind from pg_statistic_ext where stxname like 'es%' order by 1;
-- Before ANALYZE: no data, but the expressions are listed.
select count(*) from pg_statistic_ext_data;
select statistics_name, expr, inherited, null_frac from pg_stats_ext_exprs where statistics_name like 'es%' order by 1, 2;
select count(*) from pg_stats_ext;
analyze es;
select s.stxname, d.stxdinherit, d.stxdndistinct, d.stxddependencies, d.stxdmcv from pg_statistic_ext s join pg_statistic_ext_data d on d.stxoid = s.oid where s.stxname like 'es%' order by 1;
select s.stxname, m.* from pg_statistic_ext s join pg_statistic_ext_data d on d.stxoid = s.oid, pg_mcv_list_items(d.stxdmcv) m where s.stxname like 'es%' order by 1, 2;
select statistics_name, attnames, exprs, kinds, inherited, n_distinct, dependencies, most_common_vals, most_common_val_nulls, most_common_freqs, most_common_base_freqs from pg_stats_ext where statistics_name like 'es%' order by 1;
select statistics_name, expr, inherited, null_frac, avg_width, n_distinct, most_common_vals, most_common_freqs, histogram_bounds, correlation from pg_stats_ext_exprs where statistics_name like 'es%' order by 1, 2;
select s.stxname, d.stxdexpr from pg_statistic_ext s join pg_statistic_ext_data d on d.stxoid = s.oid where s.stxname like 'es%' order by 1;
select stxdndistinct::bytea, stxddependencies::text, pg_typeof(stxdmcv), pg_typeof(stxdexpr) from pg_statistic_ext_data d join pg_statistic_ext s on d.stxoid = s.oid where stxname = 'es1';
select '{}'::pg_ndistinct;
-- Objects over columns not analyzed are skipped with a warning; a target of 0 skips too.
analyze es (a, b);
analyze es (a);
alter statistics es1 set statistics 0;
alter table es alter column a set statistics 2;
analyze es;
select statistics_name, most_common_vals, most_common_freqs from pg_stats_ext where statistics_name like 'es%' order by 1;
drop table es;

-- Data types in MCV lists, too-wide values, domains and expressions.
create domain es_pos as int check (value > 0);
create table et (p es_pos, v varchar(10), d date, u uuid, f float8, t text, ts timestamptz, iv interval, n numeric(10,2), c char(3), bo bool);
insert into et select 1 + i % 4, 'k' || (i % 5), date '2024-01-01' + (i % 6), ('00000000-0000-0000-0000-00000000000' || (i % 4))::uuid, (i % 3) / 2.0, case when i % 50 = 0 then repeat('w', 1100) else 't' || (i % 2) end, timestamptz '2024-01-01 00:00:00+00' + (i % 3) * interval '1 hour', (i % 2) * interval '1 day', (i % 7) * 1.25, 'c' || (i % 2), i % 3 = 0 from generate_series(1, 300) i;
create statistics et1 (mcv) on d, u, f, t from et;
create statistics et2 (mcv, ndistinct) on ts, iv, n, c, bo from et;
create statistics et3 on (v || 'x'), (p + 1), (upper(c)) from et;
create statistics et4 (dependencies) on p, v, c from et;
analyze et;
select s.stxname, d.stxdndistinct, d.stxddependencies, d.stxdmcv from pg_statistic_ext s join pg_statistic_ext_data d on d.stxoid = s.oid where s.stxname like 'et%' order by 1;
select s.stxname, m.* from pg_statistic_ext s join pg_statistic_ext_data d on d.stxoid = s.oid, pg_mcv_list_items(d.stxdmcv) m where s.stxname like 'et%' order by 1, 2;
select statistics_name, expr, inherited, null_frac, avg_width, n_distinct, most_common_vals, most_common_freqs, histogram_bounds, correlation, most_common_elems from pg_stats_ext_exprs where statistics_name like 'et%' order by 1, 2;
select statistics_name, kinds, exprs, attnames from pg_stats_ext where statistics_name like 'et%' order by 1;
-- ALTER COLUMN TYPE makes the objects over the column anew (no data, the default target).
alter statistics et4 set statistics 10;
comment on statistics et4 is 'deps';
alter table et alter column v type text;
select s.stxname, s.stxstattarget, obj_description(s.oid, 'pg_statistic_ext'), d.stxdinherit, d.stxdndistinct is not null from pg_statistic_ext s left join pg_statistic_ext_data d on d.stxoid = s.oid where s.stxname like 'et%' order by 1, 2;
create table et_like (like et including statistics);
select count(*) from pg_statistic_ext_data d join pg_statistic_ext s on d.stxoid = s.oid where s.stxrelid = 'et_like'::regclass;
-- An empty table builds nothing (the old data stays).
truncate et;
analyze et;
select count(*) from pg_statistic_ext_data d join pg_statistic_ext s on d.stxoid = s.oid where s.stxrelid = 'et'::regclass;
drop table et, et_like;
drop domain es_pos;

-- Inheritance trees and partitions: the tree's statistics (stxdinherit).
create table ep (a int, b int);
create table ec () inherits (ep);
insert into ep select i % 4, i % 2 from generate_series(1, 40) i;
insert into ec select i % 3, i % 3 from generate_series(1, 30) i;
create statistics ep1 on a, b from ep;
analyze ep;
select s.stxname, d.stxdinherit, d.stxdndistinct, d.stxddependencies, d.stxdmcv from pg_statistic_ext s join pg_statistic_ext_data d on d.stxoid = s.oid where s.stxname = 'ep1' order by 1, 2;
select statistics_name, inherited, n_distinct from pg_stats_ext where statistics_name = 'ep1' order by 2;
alter table ep rename column b to bb;
alter statistics ep1 rename to ep2;
select s.stxname, d.stxdinherit, d.stxdndistinct is not null from pg_statistic_ext s join pg_statistic_ext_data d on d.stxoid = s.oid where s.stxname like 'ep%' order by 1, 2;
alter table ep drop column bb;
select count(*) from pg_statistic_ext where stxname like 'ep%';
drop table ep cascade;
create table ept (a int, b text) partition by range (a);
create table ept1 partition of ept for values from (0) to (5);
create table ept2 partition of ept for values from (5) to (10);
insert into ept select i % 10, 'x' || (i % 3) from generate_series(1, 50) i;
create statistics ept_s on a, b from ept;
create statistics ept1_s on a, b from ept1;
analyze ept;
select s.stxname, d.stxdinherit, d.stxdndistinct, d.stxddependencies from pg_statistic_ext s join pg_statistic_ext_data d on d.stxoid = s.oid where s.stxname like 'ept%' order by 1, 2;
drop table ept;

-- Who may see what.
create role es_user;
create table eo (a int, b int);
insert into eo select i % 3, i % 3 from generate_series(1, 30) i;
create statistics eo1 on a, b from eo;
analyze eo;
grant select on eo to es_user;
set role es_user;
select count(*) from pg_stats_ext where statistics_name = 'eo1';
select count(*) from pg_stats_ext_exprs where statistics_name = 'eo1';
select count(*) from pg_statistic_ext_data;
reset role;
alter table eo owner to es_user;
set role es_user;
select statistics_name, n_distinct from pg_stats_ext where statistics_name = 'eo1';
reset role;
drop table eo;
drop role es_user;
