-- A pg_class scan whose filter has conditions on its name index reads as the index scan
-- EXPLAIN shows: the index conditions select the rows first and the rest of the filter runs
-- on those alone, wherever it is written (a subplan that fails on any other row doesn't run
-- on it).
CREATE SCHEMA a188_ic;
CREATE TABLE a188_ic.a188_t1 (id int PRIMARY KEY, a text);
CREATE TABLE a188_ic.a188_t2 (id int);
CREATE VIEW a188_ic.a188_v AS SELECT 1 AS one;
-- EF Core's tables query, one table: NOT EXISTS over pg_depend with an InitPlan, before
-- the name.
SELECT ns.nspname, cls.relname, cls.relkind
FROM pg_class AS cls
JOIN pg_namespace AS ns ON ns.oid = cls.relnamespace
LEFT OUTER JOIN pg_description AS des ON des.objoid = cls.oid AND des.objsubid = 0
WHERE cls.relkind IN ('r', 'v', 'm', 'f', 'p')
  AND ns.nspname NOT IN ('pg_catalog', 'information_schema')
  AND NOT EXISTS (
      SELECT * FROM pg_depend AS dep
      WHERE dep.classid = (SELECT cls.oid FROM pg_class AS cls JOIN pg_namespace AS ns ON ns.oid = cls.relnamespace WHERE relname = 'pg_class' AND ns.nspname = 'pg_catalog')
        AND dep.objid = cls.oid AND dep.deptype IN ('e', 'x'))
  AND cls.relname IN ('a188_t1')
ORDER BY 2;
-- The subplan fails on every relation but a188_t1.
SELECT c.relname FROM pg_class c
WHERE NOT EXISTS (SELECT 1 FROM pg_depend d
                  WHERE d.classid = (SELECT 'pg_class'::regclass::oid) AND d.objid = c.oid
                    AND 1 / (CASE WHEN c.relname = 'a188_t1' THEN 1 ELSE 0 END) = 0)
  AND c.relname IN ('a188_t1');
SELECT c.relname FROM pg_class c
WHERE NOT EXISTS (SELECT 1 FROM pg_depend d
                  WHERE d.classid = (SELECT 'pg_class'::regclass::oid) AND d.objid = c.oid
                    AND 1 / (CASE WHEN c.relname = 'a188_t1' THEN 1 ELSE 0 END) = 0)
  AND c.relname = 'a188_t1' AND c.relnamespace = 'a188_ic'::regnamespace;
-- An anchored pattern's fixed prefix (psql's \d a188_t1 without a schema): `relname =
-- 'a188_t1'::text`.
SELECT c.relname FROM pg_class c
WHERE NOT EXISTS (SELECT 1 FROM pg_depend d
                  WHERE d.classid = (SELECT 'pg_class'::regclass::oid) AND d.objid = c.oid
                    AND 1 / (CASE WHEN c.relname = 'a188_t1' THEN 1 ELSE 0 END) = 0)
  AND c.relname OPERATOR(pg_catalog.~) '^(a188_t1)$' COLLATE pg_catalog.default;
-- A partial prefix: the range `relname >= 'a188_t'` and `< 'a188_u'`.
SELECT c.relname FROM pg_class c
WHERE NOT EXISTS (SELECT 1 FROM pg_depend d
                  WHERE d.classid = (SELECT 'pg_class'::regclass::oid) AND d.objid = c.oid
                    AND 1 / (CASE WHEN c.relname LIKE 'a188_t%' THEN 1 ELSE 0 END) = 0)
  AND c.relname OPERATOR(pg_catalog.~) '^(a188_t.*)$' COLLATE pg_catalog.default
ORDER BY 1;
SELECT c.relname FROM pg_class c
WHERE NOT EXISTS (SELECT 1 FROM pg_depend d
                  WHERE d.classid = (SELECT 'pg_class'::regclass::oid) AND d.objid = c.oid
                    AND 1 / (CASE WHEN c.relname LIKE 'a188\_t%' THEN 1 ELSE 0 END) = 0)
  AND c.relname LIKE 'a188\_t%'
ORDER BY 1;
-- psql's lookup of \d a188_t1, \d a188_t, \d a188_t*: not visible on the search path, then
-- visible.
SELECT c.relname, n.nspname FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relname OPERATOR(pg_catalog.~) '^(a188_t1)$' COLLATE pg_catalog.default
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 2, 1;
SET search_path = a188_ic, public;
SELECT c.relname, n.nspname FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relname OPERATOR(pg_catalog.~) '^(a188_t1)$' COLLATE pg_catalog.default
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 2, 1;
SELECT c.relname FROM pg_catalog.pg_class c
WHERE c.relname OPERATOR(pg_catalog.~) '^(a188_t)$' COLLATE pg_catalog.default
  AND pg_catalog.pg_table_is_visible(c.oid);
SELECT c.relname FROM pg_catalog.pg_class c
WHERE c.relname OPERATOR(pg_catalog.~) '^(a188_t.*)$' COLLATE pg_catalog.default
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1;
SELECT c.relname FROM pg_catalog.pg_class c
WHERE c.relname OPERATOR(pg_catalog.~) '^(a188_.*_pkey)$' COLLATE pg_catalog.default
ORDER BY 1;
RESET search_path;
-- A pattern without a fixed prefix, and a case-insensitive one: no bounds.
SELECT c.relname FROM pg_class c
WHERE c.relname ~ '(a188_t1|a188_v)$' AND c.relnamespace = 'a188_ic'::regnamespace ORDER BY 1;
SELECT c.relname FROM pg_class c WHERE c.relname ~* '^(A188_T1)$';
DROP SCHEMA a188_ic CASCADE;
