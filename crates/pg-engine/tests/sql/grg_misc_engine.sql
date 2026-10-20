-- Gaps closed in the regression sweep: catalog foreign keys, operators returning sets,
-- hashed NOT IN without a collation, record_image_ops uniqueness, SIRead locks, TOAST
-- tables read directly, MERGE into pg_class, deparsed RETURNING sublinks over OLD / NEW.
SELECT count(*) FROM pg_get_catalog_foreign_keys();
SELECT fktable, fkcols, pktable, pkcols, is_array, is_opt FROM pg_get_catalog_foreign_keys() WHERE fktable IN ('pg_proc'::regclass, 'pg_constraint'::regclass) ORDER BY 1::text, 2::text;
SELECT count(*) FROM pg_get_catalog_foreign_keys() WHERE is_array;
-- an operator whose function returns a set
CREATE OPERATOR |@| (PROCEDURE = unnest, RIGHTARG = anyarray);
SELECT |@| ARRAY[1, 2, 3];
SELECT |@| ARRAY['x', 'y'] AS v, 1 AS k;
DROP OPERATOR |@| (none, anyarray);
-- NOT IN over columns with conflicting implicit collations, hashed
CREATE TABLE grg_ct (a int, x text COLLATE "C", y text COLLATE "POSIX");
INSERT INTO grg_ct VALUES (1, 'a', 'b'), (2, 'b', 'a');
SELECT * FROM grg_ct WHERE (x, y) NOT IN (SELECT y, x FROM grg_ct);
SELECT * FROM grg_ct WHERE (x COLLATE "POSIX", y COLLATE "C") NOT IN (SELECT y, x FROM grg_ct);
SELECT * FROM grg_ct WHERE (x, y) NOT IN (SELECT y COLLATE "C", x COLLATE "POSIX" FROM grg_ct);
-- a unique index under record_image_ops tells 1.0 and 1.00 apart
CREATE TYPE grg_avg AS (x numeric);
CREATE TABLE grg_opf (a grg_avg NOT NULL, b text);
CREATE UNIQUE INDEX ON grg_opf (a record_image_ops);
INSERT INTO grg_opf VALUES (row(1.0)::grg_avg, 'X'), (row(1.00)::grg_avg, 'Y');
INSERT INTO grg_opf VALUES (row(1.0)::grg_avg, 'Z');
SELECT a, b FROM grg_opf GROUP BY a, b ORDER BY b;
-- serializable reads take SIRead locks
CREATE TABLE grg_tid (a int);
INSERT INTO grg_tid VALUES (1), (2), (3);
BEGIN ISOLATION LEVEL SERIALIZABLE;
SELECT * FROM grg_tid WHERE ctid = '(0,1)';
SELECT locktype, relation::regclass, page, tuple, mode FROM pg_locks WHERE pid = pg_backend_pid() AND mode = 'SIReadLock';
SELECT * FROM grg_tid WHERE ctid = ANY ('{"(0,2)","(0,3)"}');
SELECT locktype, relation::regclass, page, tuple, mode FROM pg_locks WHERE pid = pg_backend_pid() AND mode = 'SIReadLock';
SELECT count(*) FROM grg_tid;
SELECT locktype, relation::regclass, page, tuple, mode FROM pg_locks WHERE pid = pg_backend_pid() AND mode = 'SIReadLock';
ROLLBACK;
BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY;
SELECT count(*) FROM grg_tid;
SELECT count(*) FROM pg_locks WHERE pid = pg_backend_pid() AND mode = 'SIReadLock';
ROLLBACK;
-- a TOAST table read directly
CREATE TABLE grg_chunk (a text, b text STORAGE EXTERNAL);
INSERT INTO grg_chunk VALUES ('x', repeat('x', 8192));
CREATE FUNCTION grg_chunks(OUT chunk_id oid, OUT chunk_seq int, OUT len int) RETURNS SETOF record LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY EXECUTE format('SELECT chunk_id, chunk_seq, length(chunk_data) FROM %s ORDER BY 2',
    (SELECT reltoastrelid::regclass FROM pg_class WHERE relname = 'grg_chunk'));
END $$;
SELECT pg_column_toast_chunk_id(a) IS NULL, pg_column_toast_chunk_id(b) IN (SELECT chunk_id FROM grg_chunks()) FROM grg_chunk;
SELECT chunk_seq, len FROM grg_chunks();
DROP FUNCTION grg_chunks();
-- MERGE into pg_class and into a view of it
MERGE INTO pg_class c USING (SELECT 'pg_depend'::regclass AS oid) AS j ON j.oid = c.oid
WHEN MATCHED THEN UPDATE SET reltuples = reltuples + 1 RETURNING j.oid;
CREATE VIEW grg_classv AS SELECT * FROM pg_class;
MERGE INTO grg_classv c USING pg_namespace n ON n.oid = c.relnamespace
WHEN MATCHED AND c.oid = 'pg_depend'::regclass THEN UPDATE SET reltuples = reltuples - 1 RETURNING c.oid;
MERGE INTO pg_class c USING (SELECT 'grg_tid'::regclass AS oid) AS j ON j.oid = c.oid
WHEN MATCHED THEN UPDATE SET reltuples = 42, relpages = 7;
SELECT relname, reltuples, relpages FROM pg_class WHERE relname = 'grg_tid';
-- a BEGIN ATOMIC body whose RETURNING has sublinks over OLD / NEW
CREATE TABLE grg_foo (f1 int, f2 text, f3 int, f4 int);
CREATE FUNCTION grg_foo_update() RETURNS void LANGUAGE sql
BEGIN ATOMIC
  WITH u1 AS (UPDATE grg_foo SET f1 = f1 + 1 RETURNING old.*, new.*)
  UPDATE grg_foo SET f1 = f1 + 1
    RETURNING WITH (OLD AS o, NEW AS n) o.*, n.*, o.f1 = n.f1,
              (SELECT o.f2 = n.f2),
              (SELECT count(*) FROM grg_foo WHERE grg_foo.f1 = o.f4),
              (SELECT count(*) FROM grg_foo WHERE grg_foo = n);
END;
SELECT pg_get_functiondef('grg_foo_update'::regproc);
-- debug_parallel_query: a parallel-safe query runs in a worker, whose errors say so
CREATE FUNCTION grg_div(int) RETURNS int LANGUAGE sql PARALLEL SAFE AS $$ SELECT 1 / $1 $$;
SET debug_parallel_query = on;
SELECT 1 / (a - 1) FROM grg_tid;
SELECT grg_div(0);
SELECT grg_div(a) FROM grg_tid WHERE a = 1;
SET debug_parallel_query = regress;
SELECT grg_div(0);
RESET debug_parallel_query;
SELECT grg_div(0);
DROP FUNCTION grg_div(int);
DROP FUNCTION grg_foo_update();
DROP VIEW grg_classv;
DROP TABLE grg_ct, grg_opf, grg_tid, grg_chunk, grg_foo;
DROP TYPE grg_avg;
