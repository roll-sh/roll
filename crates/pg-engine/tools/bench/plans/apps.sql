-- Application, driver and ORM statements for planning CPU (tools/bench/plancpu.py) and plan
-- shapes (plancmp.py --sets apps), over plans/apps.setup.sql's schema and pgbench's tables.

-- name: liquibase_column_exists
SELECT 1 FROM pg_attribute a WHERE EXISTS (SELECT 1 FROM pg_class JOIN pg_catalog.pg_namespace ns
  ON ns.oid = pg_class.relnamespace WHERE lower(ns.nspname) = 'public' AND lower(relname) = 't77'
  AND pg_class.oid = a.attrelid) AND lower(a.attname) = 'c5'

-- name: pgjdbc_get_tables
SELECT NULL AS TABLE_CAT, n.nspname AS TABLE_SCHEM, c.relname AS TABLE_NAME,
  CASE n.nspname ~ '^pg_' OR n.nspname = 'information_schema' WHEN true THEN CASE
  WHEN n.nspname = 'pg_catalog' OR n.nspname = 'information_schema' THEN CASE c.relkind
  WHEN 'r' THEN 'SYSTEM TABLE' WHEN 'v' THEN 'SYSTEM VIEW' WHEN 'i' THEN 'SYSTEM INDEX' ELSE NULL END
  WHEN n.nspname = 'pg_toast' THEN CASE c.relkind WHEN 'r' THEN 'SYSTEM TOAST TABLE'
  WHEN 'i' THEN 'SYSTEM TOAST INDEX' ELSE NULL END ELSE CASE c.relkind WHEN 'r' THEN 'TEMPORARY TABLE'
  WHEN 'p' THEN 'TEMPORARY TABLE' WHEN 'i' THEN 'TEMPORARY INDEX' WHEN 'S' THEN 'TEMPORARY SEQUENCE'
  WHEN 'v' THEN 'TEMPORARY VIEW' ELSE NULL END END WHEN false THEN CASE c.relkind WHEN 'r' THEN 'TABLE'
  WHEN 'p' THEN 'PARTITIONED TABLE' WHEN 'i' THEN 'INDEX' WHEN 'P' then 'PARTITIONED INDEX'
  WHEN 'S' THEN 'SEQUENCE' WHEN 'v' THEN 'VIEW' WHEN 'c' THEN 'TYPE' WHEN 'f' THEN 'FOREIGN TABLE'
  WHEN 'm' THEN 'MATERIALIZED VIEW' ELSE NULL END ELSE NULL END AS TABLE_TYPE, d.description AS REMARKS,
  '' as TYPE_CAT, '' as TYPE_SCHEM, '' as TYPE_NAME, '' AS SELF_REFERENCING_COL_NAME, '' AS REF_GENERATION
  FROM pg_catalog.pg_namespace n, pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_description d
  ON (c.oid = d.objoid AND d.objsubid = 0 and d.classoid = 'pg_class'::regclass)
  WHERE c.relnamespace = n.oid AND c.relname LIKE 't77' AND (false OR ( c.relkind IN ('r','p')
  AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' ) OR ( c.relkind = 'v'
  AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' ) )
  ORDER BY TABLE_TYPE,TABLE_SCHEM,TABLE_NAME

-- name: pgjdbc_get_columns
SELECT * FROM (SELECT n.nspname,c.relname,a.attname,a.atttypid,a.attnotnull OR (t.typtype = 'd' AND t.typnotnull)
  AS attnotnull,a.atttypmod,a.attlen,t.typtypmod,row_number() OVER (PARTITION BY a.attrelid ORDER BY a.attnum)
  AS attnum, nullif(a.attidentity, '') as attidentity,nullif(a.attgenerated, '') as attgenerated,
  pg_catalog.pg_get_expr(def.adbin, def.adrelid) AS adsrc,dsc.description,t.typbasetype,t.typtype
  FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_class c ON (c.relnamespace = n.oid)
  JOIN pg_catalog.pg_attribute a ON (a.attrelid=c.oid) JOIN pg_catalog.pg_type t ON (a.atttypid = t.oid)
  LEFT JOIN pg_catalog.pg_attrdef def ON (a.attrelid=def.adrelid AND a.attnum = def.adnum)
  LEFT JOIN pg_catalog.pg_description dsc ON (c.oid=dsc.objoid AND a.attnum = dsc.objsubid)
  LEFT JOIN pg_catalog.pg_class dc ON (dc.oid=dsc.classoid AND dc.relname='pg_class')
  LEFT JOIN pg_catalog.pg_namespace dn ON (dc.relnamespace=dn.oid AND dn.nspname='pg_catalog')
  WHERE c.relkind in ('r','p','v','f','m') and a.attnum > 0 AND NOT a.attisdropped
  AND n.nspname LIKE 'public' AND c.relname LIKE 't77') c WHERE true ORDER BY nspname,c.relname,attnum

-- name: pgjdbc_get_primary_keys
SELECT result.TABLE_CAT, result.TABLE_SCHEM, result.TABLE_NAME, result.COLUMN_NAME, result.KEY_SEQ,
  result.PK_NAME FROM (SELECT NULL AS TABLE_CAT, n.nspname AS TABLE_SCHEM, ct.relname AS TABLE_NAME,
  a.attname AS COLUMN_NAME, (information_schema._pg_expandarray(i.indkey)).n AS KEY_SEQ, ci.relname AS PK_NAME,
  information_schema._pg_expandarray(i.indkey) AS KEYS, a.attnum AS A_ATTNUM FROM pg_catalog.pg_class ct
  JOIN pg_catalog.pg_attribute a ON (ct.oid = a.attrelid) JOIN pg_catalog.pg_namespace n ON (ct.relnamespace = n.oid)
  JOIN pg_catalog.pg_index i ON ( a.attrelid = i.indrelid) JOIN pg_catalog.pg_class ci ON (ci.oid = i.indexrelid)
  WHERE true AND n.nspname = E'public' AND ct.relname = E't77' AND i.indisprimary ) result
  where result.A_ATTNUM = (result.KEYS).x ORDER BY result.table_name, result.pk_name, result.key_seq

-- name: pgjdbc_get_imported_keys
SELECT NULL::text AS PKTABLE_CAT, pkn.nspname AS PKTABLE_SCHEM, pkc.relname AS PKTABLE_NAME,
  pka.attname AS PKCOLUMN_NAME, NULL::text AS FKTABLE_CAT, fkn.nspname AS FKTABLE_SCHEM, fkc.relname AS FKTABLE_NAME,
  fka.attname AS FKCOLUMN_NAME, pos.n AS KEY_SEQ, CASE con.confupdtype WHEN 'c' THEN 0 WHEN 'n' THEN 2
  WHEN 'd' THEN 4 WHEN 'r' THEN 1 WHEN 'p' THEN 1 WHEN 'a' THEN 3 ELSE NULL END AS UPDATE_RULE,
  CASE con.confdeltype WHEN 'c' THEN 0 WHEN 'n' THEN 2 WHEN 'd' THEN 4 WHEN 'r' THEN 1 WHEN 'p' THEN 1
  WHEN 'a' THEN 3 ELSE NULL END AS DELETE_RULE, con.conname AS FK_NAME, pkic.relname AS PK_NAME,
  CASE WHEN con.condeferrable AND con.condeferred THEN 5 WHEN con.condeferrable THEN 6 ELSE 7 END AS DEFERRABILITY
  FROM pg_catalog.pg_namespace pkn, pg_catalog.pg_class pkc, pg_catalog.pg_attribute pka,
  pg_catalog.pg_namespace fkn, pg_catalog.pg_class fkc, pg_catalog.pg_attribute fka, pg_catalog.pg_constraint con,
  pg_catalog.generate_series(1, 32) pos(n), pg_catalog.pg_class pkic WHERE pkn.oid = pkc.relnamespace
  AND pkc.oid = pka.attrelid AND pka.attnum = con.confkey[pos.n] AND con.confrelid = pkc.oid
  AND fkn.oid = fkc.relnamespace AND fkc.oid = fka.attrelid AND fka.attnum = con.conkey[pos.n]
  AND con.conrelid = fkc.oid AND con.contype = 'f' AND (pkic.relkind = 'i' OR pkic.relkind = 'I')
  AND pkic.oid = con.conindid AND fkn.nspname = E'public' AND fkc.relname = E't77'
  ORDER BY pkn.nspname,pkc.relname, con.conname,pos.n

-- name: pgjdbc_get_index_info
SELECT tmp.TABLE_CAT, tmp.TABLE_SCHEM, tmp.TABLE_NAME, tmp.NON_UNIQUE, tmp.INDEX_QUALIFIER, tmp.INDEX_NAME,
  tmp.TYPE, tmp.ORDINAL_POSITION, trim(both '"' from pg_catalog.pg_get_indexdef(tmp.CI_OID, tmp.ORDINAL_POSITION, false))
  AS COLUMN_NAME, CASE tmp.AM_NAME WHEN 'btree' THEN CASE tmp.I_INDOPTION[tmp.ORDINAL_POSITION - 1] & 1::smallint
  WHEN 1 THEN 'D' ELSE 'A' END ELSE NULL END AS ASC_OR_DESC, tmp.CARDINALITY, tmp.PAGES, tmp.FILTER_CONDITION
  FROM (SELECT NULL AS TABLE_CAT, n.nspname AS TABLE_SCHEM, ct.relname AS TABLE_NAME, NOT i.indisunique AS NON_UNIQUE,
  NULL AS INDEX_QUALIFIER, ci.relname AS INDEX_NAME, CASE i.indisclustered WHEN true THEN 1 ELSE CASE am.amname
  WHEN 'hash' THEN 2 ELSE 3 END END AS TYPE, (information_schema._pg_expandarray(i.indkey)).n AS ORDINAL_POSITION,
  ci.reltuples AS CARDINALITY, ci.relpages AS PAGES, pg_catalog.pg_get_expr(i.indpred, i.indrelid) AS FILTER_CONDITION,
  ci.oid AS CI_OID, i.indoption AS I_INDOPTION, am.amname AS AM_NAME FROM pg_catalog.pg_class ct
  JOIN pg_catalog.pg_namespace n ON (ct.relnamespace = n.oid) JOIN pg_catalog.pg_index i ON (ct.oid = i.indrelid)
  JOIN pg_catalog.pg_class ci ON (ci.oid = i.indexrelid) JOIN pg_catalog.pg_am am ON (ci.relam = am.oid)
  WHERE true AND n.nspname = E'public' AND ct.relname = E't77') AS tmp
  ORDER BY NON_UNIQUE, TYPE, INDEX_NAME, ORDINAL_POSITION

-- name: kysely_get_tables
select "a"."attname" as "column", "a"."attnotnull" as "not_null", "a"."atthasdef" as "has_default",
  "c"."relname" as "table", "c"."relkind" as "table_type", "ns"."nspname" as "schema", "typ"."typname" as "type",
  "dtns"."nspname" as "type_schema", col_description(a.attrelid, a.attnum) as column_description,
  pg_get_serial_sequence(quote_ident(ns.nspname) || '.' || quote_ident(c.relname), a.attname) as auto_incrementing
  from "pg_catalog"."pg_attribute" as "a" inner join "pg_catalog"."pg_class" as "c" on "a"."attrelid" = "c"."oid"
  inner join "pg_catalog"."pg_namespace" as "ns" on "c"."relnamespace" = "ns"."oid"
  inner join "pg_catalog"."pg_type" as "typ" on "a"."atttypid" = "typ"."oid"
  inner join "pg_catalog"."pg_namespace" as "dtns" on "typ"."typnamespace" = "dtns"."oid"
  where "c"."relkind" in ('r', 'v', 'p') and "ns"."nspname" !~ '^pg_' and "ns"."nspname" != 'information_schema'
  and "a"."attnum" >= 0 and "a"."attisdropped" != true order by "ns"."nspname", "c"."relname", "a"."attnum"

-- name: pgwatch_table_stats_approx
SELECT c.relname, c.reltuples, c.relpages FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  WHERE c.relkind = 'r' AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname !~ '^pg_toast'

-- name: oban_fetch
UPDATE oban_jobs SET state = 'executing', attempted_at = now(), attempt = attempt + 1
  WHERE id IN (SELECT id FROM oban_jobs WHERE state = 'available' AND queue = 'default'
  ORDER BY priority, scheduled_at, id LIMIT 10 FOR UPDATE SKIP LOCKED) RETURNING id

-- name: graphile_fetch
WITH j AS (SELECT id FROM gw_jobs WHERE locked_at IS NULL AND run_at <= now() AND attempts < max_attempts
  ORDER BY priority, run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED) UPDATE gw_jobs SET attempts = gw_jobs.attempts + 1,
  locked_at = now(), locked_by = 'w' FROM j WHERE gw_jobs.id = j.id RETURNING gw_jobs.id

-- name: que_fetch
SELECT * FROM que_jobs WHERE queue = 'default' AND finished_at IS NULL AND expired_at IS NULL
  AND run_at <= now() ORDER BY priority, run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED

-- name: pgbench_select
SELECT abalance FROM pgbench_accounts WHERE aid = 4711

-- name: pgbench_update_accounts
UPDATE pgbench_accounts SET abalance = abalance + 5 WHERE aid = 4711

-- name: pgbench_update_tellers
UPDATE pgbench_tellers SET tbalance = tbalance + 5 WHERE tid = 7

-- name: pgbench_update_branches
UPDATE pgbench_branches SET bbalance = bbalance + 5 WHERE bid = 3

-- name: pgbench_insert_history
INSERT INTO pgbench_history (tid, bid, aid, delta, mtime) VALUES (7, 3, 4711, 5, CURRENT_TIMESTAMP)
