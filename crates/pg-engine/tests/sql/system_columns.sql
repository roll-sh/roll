-- System columns: tableoid, ctid, xmin, xmax, cmin, cmax.
CREATE TABLE s (id int PRIMARY KEY, n int, g text);
INSERT INTO s VALUES (1, 3, 'a'), (2, 4, 'b') RETURNING id, xmax, cmin, tableoid = 's'::regclass AS same_table;
SELECT tableoid::regclass, id FROM s ORDER BY id;
SELECT s.tableoid::regclass, s.xmax, s.cmax FROM s ORDER BY id;
SELECT xmin <> 0 AS has_xmin, ctid IS NOT NULL AS has_ctid FROM s ORDER BY id;
SELECT pg_typeof(ctid), pg_typeof(xmin), pg_typeof(cmin), pg_typeof(tableoid) FROM s LIMIT 1;

-- The upsert idiom: xmax = 0 means the row was inserted.
INSERT INTO s VALUES (1, 9, 'a'), (3, 5, 'c') ON CONFLICT (id) DO UPDATE SET n = excluded.n RETURNING id, n, (xmax = 0) AS inserted;
UPDATE s SET n = n + 1 WHERE id = 2 RETURNING n, xmax;

-- Same transaction: xmin is our xid.
BEGIN;
INSERT INTO s VALUES (10, 1, 'z');
SELECT id, xmin = pg_current_xact_id()::text::xid AS mine FROM s WHERE id >= 3 ORDER BY id;
COMMIT;

-- ctid identifies rows: delete duplicates.
CREATE TABLE r (a int);
INSERT INTO r VALUES (1), (1), (2), (2), (2), (3);
DELETE FROM r a USING r b WHERE a.a = b.a AND a.ctid < b.ctid;
SELECT a FROM r ORDER BY a;
SELECT a FROM r WHERE ctid = (SELECT ctid FROM r WHERE a = 2);
SELECT count(*) FROM r x JOIN r y ON x.ctid = y.ctid;

-- User columns shadow nothing; system names are reserved.
CREATE TABLE bad (xmin int);
SELECT ctid FROM s, r;
SELECT xmin FROM (SELECT 1) x;
SELECT bit_or(a), cmin FROM r GROUP BY cmin;
CREATE TABLE bad2 (a int);
ALTER TABLE bad2 ADD COLUMN ctid int;
ALTER TABLE bad2 RENAME a TO cmax;
CREATE TABLE bad3 AS SELECT 1 AS tableoid;
CREATE VIEW okv AS SELECT 1 AS xmin;
SELECT xmin FROM okv;
