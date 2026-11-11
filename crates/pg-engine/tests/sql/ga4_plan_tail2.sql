-- Round 14 (agent 4), continued: Memoize in binary mode (-0.0 and 0.0 two keys) and its
-- evictions past hash_mem, unread outputs of a subquery level as NULLs, a grouped
-- subquery unique for a join, and a non-leakproof equality under row-level security
-- kept out of equivalence classes.
CREATE FUNCTION ga4_explain_memoize(query text) RETURNS SETOF text
LANGUAGE plpgsql AS
$$
DECLARE
    ln text;
BEGIN
    FOR ln IN
        EXECUTE format('explain (analyze, costs off, summary off, timing off, buffers off) %s', query)
    LOOP
        ln := regexp_replace(ln, 'Evictions: 0', 'Evictions: Zero');
        ln := regexp_replace(ln, 'Evictions: \d+', 'Evictions: N');
        ln := regexp_replace(ln, 'Memory Usage: \d+', 'Memory Usage: N');
        ln := regexp_replace(ln, 'Heap Fetches: \d+', 'Heap Fetches: N');
        ln := regexp_replace(ln, 'loops=\d+', 'loops=N');
        ln := regexp_replace(ln, 'Index Searches: \d+', 'Index Searches: N');
        RETURN NEXT ln;
    END LOOP;
END;
$$;
CREATE TABLE ga4_flt (f float);
CREATE INDEX ga4_flt_f_idx ON ga4_flt (f);
INSERT INTO ga4_flt VALUES ('-0.0'::float), ('+0.0'::float);
ANALYZE ga4_flt;
SET enable_seqscan TO off;
SET enable_hashjoin TO off;
SET enable_mergejoin TO off;
SET enable_bitmapscan TO off;
SELECT ga4_explain_memoize('SELECT * FROM ga4_flt f1 INNER JOIN ga4_flt f2 ON f1.f >= f2.f;');
SELECT ga4_explain_memoize('SELECT * FROM ga4_flt f1 INNER JOIN ga4_flt f2 ON f1.f = f2.f;');
RESET enable_seqscan;

CREATE TABLE ga4_tk (unique1 int, two int, ten int, thousand int);
INSERT INTO ga4_tk SELECT i, i % 2, i % 10, i % 1000 FROM generate_series(0, 9999) i;
CREATE INDEX ga4_tk_u1 ON ga4_tk (unique1);
VACUUM ANALYZE ga4_tk;
SET work_mem TO '64kB';
SET hash_mem_multiplier TO 1.0;
SELECT regexp_replace(t, '(Hits|Misses): \d+', '\1: N', 'g')
FROM ga4_explain_memoize('
SELECT COUNT(*), AVG(t1.unique1) FROM ga4_tk t1
INNER JOIN ga4_tk t2 ON t1.unique1 = t2.thousand
WHERE t2.unique1 < 1200;') t;
SELECT COUNT(*), AVG(t1.unique1) FROM ga4_tk t1
INNER JOIN ga4_tk t2 ON t1.unique1 = t2.thousand
WHERE t2.unique1 < 1200;
RESET work_mem;
RESET hash_mem_multiplier;
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_bitmapscan;

-- Unread outputs of a subquery planned apart: NULLs, constants as such above.
CREATE TABLE ga4_t (a int, b int, c text);
INSERT INTO ga4_t SELECT i % 10, i % 7, 'x' || i FROM generate_series(1, 1000) i;
ANALYZE ga4_t;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT s.a FROM ga4_t t LEFT JOIN (SELECT a, b + 1 AS y FROM ga4_t ORDER BY 1 LIMIT 3) s ON s.a = t.a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1 FROM (SELECT a, b * 2 AS z FROM ga4_t OFFSET 0) s;
SELECT count(*) FROM (SELECT a, b * 2 AS z FROM ga4_t OFFSET 0) s;
-- A grouped subquery is unique on its keys.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT s.a FROM ga4_t t LEFT JOIN (SELECT a, sum(b) AS x FROM ga4_t GROUP BY a) s ON s.a = t.a;

-- Row-level security: a non-leakproof equality with a constant isn't an equivalence
-- class member.
CREATE TYPE ga4_alias;
CREATE FUNCTION ga4_alias_in(cstring) RETURNS ga4_alias STRICT IMMUTABLE LANGUAGE internal AS 'int8in';
CREATE FUNCTION ga4_alias_out(ga4_alias) RETURNS cstring STRICT IMMUTABLE LANGUAGE internal AS 'int8out';
CREATE TYPE ga4_alias (input = ga4_alias_in, output = ga4_alias_out, like = int8);
CREATE CAST (int8 AS ga4_alias) WITHOUT FUNCTION;
CREATE CAST (ga4_alias AS int8) WITHOUT FUNCTION;
CREATE FUNCTION ga4_alias_eq(int8, ga4_alias) RETURNS bool STRICT IMMUTABLE LANGUAGE internal AS 'int8eq';
CREATE OPERATOR = (procedure = ga4_alias_eq, leftarg = int8, rightarg = ga4_alias,
  restrict = eqsel, join = eqjoinsel, merges);
CREATE FUNCTION ga4_alias_lt(ga4_alias, ga4_alias) RETURNS bool STRICT IMMUTABLE LANGUAGE internal AS 'int8lt';
CREATE OPERATOR < (procedure = ga4_alias_lt, leftarg = ga4_alias, rightarg = ga4_alias);
CREATE TABLE ga4_ec0 (ff int8 PRIMARY KEY, f1 int8);
CREATE TABLE ga4_ec1 (ff int8 PRIMARY KEY, f1 ga4_alias);
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN (COSTS OFF)
  SELECT * FROM ga4_ec0 a, ga4_ec1 b WHERE a.ff = b.ff AND a.ff = 43::bigint::ga4_alias;
-- (In integer_ops, the equality is a class's.)
ALTER OPERATOR FAMILY integer_ops USING btree ADD OPERATOR 3 = (int8, ga4_alias);
CREATE FUNCTION ga4_alias_eq2(ga4_alias, ga4_alias) RETURNS bool STRICT IMMUTABLE LANGUAGE internal AS 'int8eq';
CREATE OPERATOR = (procedure = ga4_alias_eq2, leftarg = ga4_alias, rightarg = ga4_alias,
  commutator = =, restrict = eqsel, join = eqjoinsel, merges);
ALTER OPERATOR FAMILY integer_ops USING btree ADD OPERATOR 3 = (ga4_alias, ga4_alias);
EXPLAIN (COSTS OFF)
  SELECT * FROM ga4_ec0 a, ga4_ec1 b WHERE a.ff = b.ff AND a.ff = 43::bigint::ga4_alias;
ALTER TABLE ga4_ec1 ENABLE ROW LEVEL SECURITY;
CREATE POLICY p1 ON ga4_ec1 USING (f1 < '5'::ga4_alias);
DROP ROLE IF EXISTS ga4_user;
CREATE ROLE ga4_user;
GRANT SELECT ON ga4_ec0, ga4_ec1 TO ga4_user;
SET SESSION AUTHORIZATION ga4_user;
EXPLAIN (COSTS OFF)
  SELECT * FROM ga4_ec0 a, ga4_ec1 b WHERE a.ff = b.ff AND a.ff = 43::bigint::ga4_alias;
RESET SESSION AUTHORIZATION;
REVOKE SELECT ON ga4_ec0, ga4_ec1 FROM ga4_user;
DROP ROLE ga4_user;
RESET enable_hashjoin;
RESET enable_mergejoin;
