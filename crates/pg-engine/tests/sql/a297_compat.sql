-- What frameworks and tools read differently from PostgreSQL 18.6 (agent 297, tools/compat).
-- golden: raw-order
-- A catalog column one of its btree indexes leads has its actual range
-- (get_actual_variable_range): a comparison past it selects nothing, and a merge join of
-- pg_constraint.confrelid (all 0 in initdb's statistics) against pg_class.oid costs a full
-- scan, so drizzle-kit's foreign key listing fetches each referenced table by its oid.
EXPLAIN (COSTS OFF) SELECT * FROM pg_constraint WHERE oid > 4000000000;
EXPLAIN (COSTS OFF) SELECT c.oid FROM pg_constraint c JOIN pg_class r ON r.oid = c.confrelid WHERE c.contype = 'f';
-- information_schema's columns are of its domains (pg_typeof; CREATE TABLE AS takes them).
SELECT pg_typeof(table_name) AS name_t, pg_typeof(is_insertable_into) AS yes_t FROM information_schema.tables LIMIT 1;
SELECT pg_typeof(ordinal_position) AS card_t, pg_typeof(data_type) AS chr_t FROM information_schema.columns LIMIT 1;
SELECT pg_typeof(constraint_name) AS ident_t FROM information_schema.table_constraints LIMIT 1;
CREATE TABLE a297_names AS SELECT table_name, is_insertable_into FROM information_schema.tables LIMIT 0;
SELECT attname, format_type(atttypid, atttypmod) FROM pg_attribute
  WHERE attrelid = 'a297_names'::regclass AND attnum > 0 ORDER BY attnum;
-- relation_is_updatable of system relations: a catalog as a table, the auto-updatable views
-- over one catalog, pg_settings by its INSTEAD rule.
SELECT c.relname, pg_relation_is_updatable(c.oid, false) AS upd,
    pg_column_is_updatable(c.oid, 1::int2, false) AS col1, pg_column_is_updatable(c.oid, 3::int2, false) AS col3
  FROM pg_class c WHERE c.relname IN ('pg_class', 'pg_group', 'pg_settings', 'enabled_roles', 'pg_stat_database_conflicts', 'pg_roles')
  ORDER BY 1;
SELECT table_name, is_updatable, is_insertable_into FROM information_schema.views
  WHERE table_name IN ('pg_group', 'pg_stat_database_conflicts', 'pg_settings') ORDER BY 1;
-- What PostgreSQL grants nothing to PUBLIC: information_schema's _pg_* views, sql_parts,
-- transforms; pg_subscription only column by column, subconninfo not.
CREATE ROLE a297_plain;
SET ROLE a297_plain;
SELECT count(*) FROM information_schema._pg_foreign_tables;
SELECT count(*) FROM information_schema._pg_user_mappings;
SELECT count(*) FROM information_schema.sql_parts;
SELECT count(*) FROM information_schema.transforms;
SELECT count(*) FROM pg_subscription;
SELECT subname FROM pg_subscription;
SELECT subconninfo FROM pg_subscription;
SELECT count(*) FROM (SELECT s FROM pg_subscription s) x;
SELECT count(*) FROM information_schema.views WHERE table_schema = 'information_schema';
RESET ROLE;
DROP ROLE a297_plain;
-- pg_class.relpartbound is a PartitionBoundSpec node tree; pg_get_expr prints the bound.
CREATE TABLE a297_l (c text, v int) PARTITION BY LIST (c);
CREATE TABLE a297_l1 PARTITION OF a297_l FOR VALUES IN ('a', 'b', NULL);
CREATE TABLE a297_ld PARTITION OF a297_l DEFAULT;
CREATE TABLE a297_r (d date, n int, v int) PARTITION BY RANGE (d, n);
CREATE TABLE a297_r1 PARTITION OF a297_r FOR VALUES FROM ('2025-01-01', MINVALUE) TO ('2026-01-01', 200);
CREATE TABLE a297_r2 PARTITION OF a297_r FOR VALUES FROM ('2026-01-01', 200) TO (MAXVALUE, MAXVALUE);
CREATE TABLE a297_h (i int8) PARTITION BY HASH (i);
CREATE TABLE a297_h1 PARTITION OF a297_h FOR VALUES WITH (MODULUS 4, REMAINDER 3);
CREATE TABLE a297_v (c varchar(10) COLLATE "C") PARTITION BY LIST (c);
CREATE TABLE a297_v1 PARTITION OF a297_v FOR VALUES IN ('x');
SELECT relname, relpartbound, pg_get_expr(relpartbound, oid) FROM pg_class
  WHERE relname IN ('a297_l1', 'a297_ld', 'a297_r1', 'a297_r2', 'a297_h1', 'a297_v1') ORDER BY 1;
-- A node tree's datum bytes are signed chars (outDatum): 200 is -56.
CREATE TABLE a297_chk (x int CHECK (x > 200), y int8 DEFAULT 4096);
SELECT conbin FROM pg_constraint WHERE conrelid = 'a297_chk'::regclass AND contype = 'c';
