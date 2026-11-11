-- REINDEX SYSTEM goes through the catalogs in the order a pg_class heap scan returns them
-- (each catalog's TOAST index first, then its indexes in oid order): the new files' numbers
-- follow that order.
reindex table pg_class;
reindex system;
select c.relname, rank() over (order by c.relfilenode) from pg_class c where c.relkind = 'i' and c.relnamespace in ('pg_catalog'::regnamespace, 'pg_toast'::regnamespace) and c.relfilenode >= 16384 order by c.relfilenode;
