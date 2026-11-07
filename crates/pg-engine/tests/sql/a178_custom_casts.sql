-- Custom plans of text parameters cast through a STABLE input routine (CoerceViaIO), recorded
-- from PostgreSQL 18.6 (difftest rounds 9 and 10): the custom plan keeps the cast, shown over
-- the value as a cstring, estimates with its value and fails while planning on a bad one.
CREATE TABLE r9s (id int PRIMARY KEY, g int, v int, t text, x numeric);
INSERT INTO r9s SELECT i, 1, NULL, (ARRAY['a', 'a b'])[i % 8 + 1] || 1 FROM generate_series(1, 2885) i;
ANALYZE r9s;
PREPARE r9p (text) AS SELECT id, g FROM r9s WHERE id = ANY ($1::int[]) ORDER BY id;
EXECUTE r9p('{50,100,NULL}');
EXECUTE r9p('{2999,1}');
EXECUTE r9p('{}');
EXECUTE r9p('{1,2,3}');
EXPLAIN EXECUTE r9p('{1,2,3}');
EXECUTE r9p('{50,100,NULL}');
EXECUTE r9p('{50,100,NULL}');
SELECT generic_plans, custom_plans FROM pg_prepared_statements WHERE name = 'r9p';
EXPLAIN EXECUTE r9p('{1,2,3}');
DEALLOCATE r9p;

CREATE TABLE r10c (id int PRIMARY KEY, k int, d timestamptz, n date);
INSERT INTO r10c SELECT i, i % 7, timestamptz '2026-01-01 00:00:00+00' + i * interval '1 hour', date '2026-01-01' + i FROM generate_series(1, 500) i;
ANALYZE r10c;
PREPARE q1 (text, int) AS SELECT id FROM r10c WHERE k = ANY ($1::int[]) AND id < $2;
EXECUTE q1('{1,x}', 0);
EXECUTE q1('{1,2}', 9);
EXPLAIN (COSTS OFF) EXECUTE q1('{1,2}', 9);
SELECT generic_plans, custom_plans FROM pg_prepared_statements WHERE name = 'q1';
PREPARE q2 (text) AS SELECT count(*) FROM r10c WHERE d > $1::timestamptz;
EXECUTE q2('2026-02-30');
EXECUTE q2('2026-01-15');
EXPLAIN EXECUTE q2('2026-01-15');
SELECT generic_plans, custom_plans FROM pg_prepared_statements WHERE name = 'q2';
PREPARE q3 (text) AS SELECT relname FROM pg_class WHERE oid = $1::regclass;
EXECUTE q3('r10nope');
EXECUTE q3('r10c');
PREPARE q4 (text, text) AS SELECT id FROM r10c WHERE n < $1::date AND d > $2::timestamptz ORDER BY id LIMIT 3;
EXECUTE q4('2026-01-05', 'x');
EXECUTE q4('y', '2026-01-01');
EXECUTE q4('y', 'x');
EXECUTE q4('2026-01-05', '2026-01-01');
EXPLAIN (COSTS OFF) EXECUTE q4('2026-01-05', '2026-01-01');
-- Not in a restriction clause: evaluated when run.
PREPARE q5 (text) AS SELECT $1::date AS d FROM r10c WHERE id = 1;
EXPLAIN (COSTS OFF, VERBOSE) EXECUTE q5('2026-01-03');
EXECUTE q5('2026-01-03');
EXECUTE q5('nope');
DEALLOCATE ALL;
DROP TABLE r9s, r10c;
