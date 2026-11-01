-- Equality normalization must not impose bytewise range semantics on bpchar.
CREATE TABLE a144_edges (id int, c char(3), u bpchar);
INSERT INTO a144_edges VALUES (1, 'a', 'a'), (2, 'a' || chr(9), 'a '),
  (3, 'a' || chr(10), 'a  '), (4, '', ''), (5, NULL, NULL), (6, 'É', 'É ');
CREATE INDEX ON a144_edges (c DESC);
CREATE INDEX ON a144_edges (u);
SET enable_seqscan = off;
SELECT id FROM a144_edges WHERE c = ANY (ARRAY['a', 'a ', 'a    ', NULL]::bpchar[]) ORDER BY id;
SELECT id FROM a144_edges WHERE c = ANY (ARRAY['É', 'É    ', 'Éxxx']::bpchar[]) ORDER BY id;
SELECT id FROM a144_edges WHERE c = ANY (ARRAY[]::bpchar[]) ORDER BY id;
SELECT id FROM a144_edges WHERE c = ANY (ARRAY[NULL]::bpchar[]) ORDER BY id;
SELECT id FROM a144_edges WHERE c = 'axxx ' ORDER BY id;
SELECT id FROM a144_edges WHERE c < 'a' ORDER BY id;
SELECT id FROM a144_edges WHERE c > 'a' ORDER BY id;
SELECT id FROM a144_edges WHERE c BETWEEN 'a' AND 'a ' ORDER BY id;
SELECT id FROM a144_edges WHERE u = 'a   ' ORDER BY id;
SELECT id FROM a144_edges WHERE u = ANY (ARRAY['a', 'a  ', NULL]::bpchar[]) ORDER BY id;
-- An overlong probe can still compare equal under a nondeterministic collation.
CREATE COLLATION a144_ign (provider = icu, locale = 'und-u-ka-shifted-ks-level1', deterministic = false);
CREATE TABLE a144_coll (id int, c char(3) COLLATE a144_ign);
INSERT INTO a144_coll VALUES (1, 'abc'), (2, 'ABC'), (3, 'é'), (4, 'xyz');
CREATE INDEX ON a144_coll (c);
SELECT id FROM a144_coll WHERE c = 'a-b-c---' ORDER BY id;
SELECT id FROM a144_coll WHERE c = ANY (ARRAY['a-b-c---', 'ABC ', 'É---']::bpchar[]) ORDER BY id;
DROP INDEX a144_coll_c_idx;
CREATE INDEX ON a144_coll USING hash (c);
SELECT id FROM a144_coll WHERE c = 'a-b-c---' ORDER BY id;
SELECT id FROM a144_coll WHERE c = ANY (ARRAY['a-b-c---', 'ABC ', 'É---']::bpchar[]) ORDER BY id;
RESET enable_seqscan;
DROP TABLE a144_edges, a144_coll;
DROP COLLATION a144_ign;
