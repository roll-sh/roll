-- Compression methods merging through inheritance (a parent without one set takes the
-- other's), a local definition against the inherited one, and materialized views' SET
-- COMPRESSION / SET STORAGE.
create table ci_p1 (f1 text compression pglz);
create table ci_p2 (f1 text compression lz4);
create table ci_p3 (f1 text);
create table ci_c1 () inherits (ci_p1, ci_p2);
create table ci_c2 (f1 text compression lz4) inherits (ci_p1);
create table ci_c3 () inherits (ci_p1, ci_p3);
create table ci_c4 () inherits (ci_p3, ci_p2);
create table ci_c5 (f1 text) inherits (ci_p2);
create table ci_c6 (f1 text compression pglz) inherits (ci_p3);
create table ci_c7 (f1 text storage external) inherits (ci_p1);
create table ci_c8 (f1 text storage main) inherits (ci_p3);
select c.relname, a.attcompression, a.attstorage from pg_attribute a join pg_class c on c.oid = a.attrelid
  where c.relname like 'ci\_%' and a.attname = 'f1' order by 1;
create materialized view ci_mv (x, y) as select f1, 1 from ci_p1;
alter materialized view ci_mv alter column x set compression lz4;
alter materialized view ci_mv alter column x set storage main;
alter materialized view ci_mv alter column y set compression lz4;
alter materialized view ci_mv alter column z set compression lz4;
alter table ci_mv alter column x set compression pglz;
select attname, attcompression, attstorage from pg_attribute where attrelid = 'ci_mv'::regclass and attnum > 0 order by attnum;
drop materialized view ci_mv;
drop table ci_p1, ci_p2, ci_p3 cascade;
