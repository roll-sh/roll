-- A join on `name = text` (nameeqtext / texteqname, both in the hash text_ops family:
-- hashname and hashtext hash equal strings alike) is hashed like any equi-join.
CREATE TABLE a19_nk (s name, v int);
CREATE TABLE a19_tx (i text, w int);
INSERT INTO a19_nk SELECT 'A' || g, g FROM generate_series(1, 1000) g;
INSERT INTO a19_tx SELECT 'A' || g, g FROM generate_series(1, 1000, 3) g;
INSERT INTO a19_tx VALUES ('A5', -5), (NULL, 0), ('a5', 7), ('A5 ', 8);
ANALYZE a19_nk;
ANALYZE a19_tx;
EXPLAIN (COSTS OFF) SELECT * FROM a19_nk nk JOIN a19_tx tx ON nk.s = tx.i;
EXPLAIN (COSTS OFF) SELECT * FROM a19_nk nk JOIN a19_tx tx ON tx.i = nk.s;
SELECT count(*), sum(v), sum(w) FROM a19_nk nk JOIN a19_tx tx ON nk.s = tx.i;
SELECT * FROM a19_nk nk JOIN a19_tx tx ON tx.i = nk.s WHERE v < 12 ORDER BY v, w;
EXPLAIN (COSTS OFF) SELECT * FROM a19_nk nk LEFT JOIN a19_tx tx ON nk.s = tx.i;
SELECT count(*), count(w) FROM a19_nk nk LEFT JOIN a19_tx tx ON nk.s = tx.i;
EXPLAIN (COSTS OFF) SELECT count(*) FROM a19_nk nk WHERE EXISTS (SELECT 1 FROM a19_tx tx WHERE tx.i = nk.s);
SELECT count(*) FROM a19_nk nk WHERE EXISTS (SELECT 1 FROM a19_tx tx WHERE tx.i = nk.s);
EXPLAIN (COSTS OFF) SELECT count(*) FROM a19_tx tx WHERE tx.i IN (SELECT s FROM a19_nk);
SELECT count(*) FROM a19_tx tx WHERE tx.i IN (SELECT s FROM a19_nk);
-- A constant the name side is equated with crosses the class (its collation C): the
-- text side compares with it by texteqname. The text side's own `= 'A5'` (its column's
-- collation) is a class of its own.
EXPLAIN (COSTS OFF) SELECT * FROM a19_nk nk JOIN a19_tx tx ON nk.s = tx.i AND nk.s = 'A5';
SELECT * FROM a19_nk nk JOIN a19_tx tx ON nk.s = tx.i AND nk.s = 'A5' ORDER BY w;
EXPLAIN (COSTS OFF) SELECT * FROM a19_nk nk JOIN a19_tx tx ON nk.s = tx.i WHERE tx.i = 'A5';
SELECT * FROM a19_nk nk JOIN a19_tx tx ON nk.s = tx.i WHERE tx.i = 'A5' ORDER BY w;
EXPLAIN (COSTS OFF) SELECT * FROM a19_tx tx JOIN a19_nk nk ON tx.i = nk.s WHERE nk.s = 'A7';
SELECT * FROM a19_tx tx JOIN a19_nk nk ON tx.i = nk.s WHERE nk.s = 'A7';
-- The text side compared with a name (under C, the class's collation) joins the class:
-- both sides get the constant, the text side's clause relabeled.
EXPLAIN (COSTS OFF) SELECT * FROM a19_tx tx JOIN a19_nk nk ON tx.i = nk.s WHERE tx.i = 'A7'::name;
SELECT * FROM a19_tx tx JOIN a19_nk nk ON tx.i = nk.s WHERE tx.i = 'A7'::name;
-- A scan's own `text = name` restriction, a class of two members from one clause: as
-- written.
EXPLAIN (COSTS OFF) SELECT * FROM a19_tx WHERE i = current_user;
EXPLAIN (COSTS OFF) SELECT * FROM a19_tx WHERE i = 'A7'::name;
EXPLAIN (COSTS OFF) SELECT * FROM a19_tx WHERE i = (w::text)::name;
SELECT * FROM a19_tx WHERE i = 'A7'::name;
