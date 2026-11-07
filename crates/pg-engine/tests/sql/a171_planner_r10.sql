-- Planner round 10 (agent 171).
-- Sort keys under a written COLLATE: the CollateExpr is a RelabelType (none when the
-- collation is the expression's own), and the key is the first select-list entry that is
-- the key's expression, relabelings stripped.
CREATE TABLE a171_e (t text, u text COLLATE "C", n name, i int);
INSERT INTO a171_e SELECT g::text, g::text, g::text, g FROM generate_series(1, 100) g;
ANALYZE a171_e;
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY t COLLATE "en-x-icu";
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY u COLLATE "en-x-icu" NULLS FIRST;
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY u COLLATE "default";
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY lower(t) COLLATE "en-x-icu";
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY (t || 'x') COLLATE "C";
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY n COLLATE "en-x-icu";
EXPLAIN (COSTS OFF) SELECT i FROM a171_e ORDER BY t COLLATE "en-x-icu";
EXPLAIN (COSTS OFF) SELECT u FROM a171_e ORDER BY u COLLATE "en-x-icu";
EXPLAIN (COSTS OFF) SELECT t, u FROM a171_e ORDER BY u COLLATE "C";
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY t COLLATE "C" DESC;
EXPLAIN (COSTS OFF) SELECT lower(t) FROM a171_e ORDER BY lower(t) COLLATE "en-x-icu";
EXPLAIN (COSTS OFF) SELECT a.t FROM a171_e a JOIN a171_e b ON a.i = b.i ORDER BY a.t COLLATE "en-x-icu";
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY u COLLATE "en-x-icu" LIMIT 3;
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY u COLLATE "C";
EXPLAIN (COSTS OFF) SELECT t FROM a171_e ORDER BY t COLLATE "default";
EXPLAIN (COSTS OFF) SELECT i FROM a171_e ORDER BY t COLLATE "default";
EXPLAIN (COSTS OFF) SELECT t, t COLLATE "en-x-icu" AS x FROM a171_e ORDER BY 2;
EXPLAIN (COSTS OFF) SELECT t COLLATE "en-x-icu" AS x, t FROM a171_e ORDER BY 1;
SELECT t FROM a171_e WHERE i < 12 ORDER BY u COLLATE "en-x-icu" DESC, i;
-- A built-in returning one composite row is planned as one row: three in one FROM nest
-- the first two, then the third (the order their slots would be made in).
EXPLAIN SELECT 'init' FROM pg_create_logical_replication_slot('a', 'test_decoding') s1,
  pg_create_logical_replication_slot('b', 'test_decoding') s2, pg_create_logical_replication_slot('c', 'test_decoding') s3;
EXPLAIN SELECT 1 FROM pg_create_logical_replication_slot('a', 'test_decoding') s1, a171_e WHERE i = 1;
-- A built-in set-returning function's rows are its prorows.
EXPLAIN SELECT * FROM pg_partition_ancestors('a171_e'::regclass);
EXPLAIN SELECT * FROM json_each('{"a": 1}');
EXPLAIN SELECT * FROM regexp_matches('abc', 'b');
EXPLAIN SELECT * FROM pg_get_keywords();
-- BETWEEN binds its bounds in PostgreSQL's order: `x >= lo`, then `hi`, so an untyped
-- parameter takes its type from its first use.
PREPARE a171_p1 AS SELECT i FROM a171_e WHERE i BETWEEN $1 AND $1 + $2 ORDER BY i;
EXECUTE a171_p1(3, 2);
PREPARE a171_p2 AS SELECT i FROM a171_e WHERE i NOT BETWEEN $1 AND $1 + $2 AND i < 6 ORDER BY i;
EXECUTE a171_p2(2, 1);
PREPARE a171_p3 AS SELECT i FROM a171_e WHERE i BETWEEN SYMMETRIC $1 AND $1 - $2 ORDER BY i;
EXECUTE a171_p3(5, 2);
SELECT parameter_types FROM pg_prepared_statements WHERE name LIKE 'a171_p%' ORDER BY name;
-- The comparison with the lower bound fails before the upper bound is bound.
SELECT 1 FROM a171_e WHERE i BETWEEN 'x'::text AND nosuch;
SELECT 1 FROM a171_e WHERE i BETWEEN 1 AND nosuch;
-- An uncorrelated IN over a set-returning sub-select (a UNION ALL arm's) is a semi join
-- with the sub-select as a subquery of its own (convert_ANY_sublink_to_join).
CREATE TABLE a171_p (id int PRIMARY KEY);
CREATE TABLE a171_c (id int PRIMARY KEY, p int REFERENCES a171_p);
INSERT INTO a171_p SELECT generate_series(1, 10);
INSERT INTO a171_c SELECT g, g % 10 + 1 FROM generate_series(1, 50) g;
ANALYZE a171_p, a171_c;
EXPLAIN (COSTS OFF) SELECT * FROM a171_c WHERE p IN (SELECT generate_series(1, 3) UNION ALL VALUES (5));
SELECT count(*) FROM a171_c WHERE p IN (SELECT generate_series(1, 3) UNION ALL VALUES (5));
EXPLAIN (COSTS OFF) SELECT * FROM a171_c WHERE p IN (SELECT pg_partition_ancestors('a171_p'::regclass)::int UNION ALL VALUES (5));
SELECT count(*) FROM a171_c WHERE p NOT IN (SELECT generate_series(1, 3) UNION ALL VALUES (5));
DROP TABLE a171_c, a171_p;
DEALLOCATE ALL;
DROP TABLE a171_e;
