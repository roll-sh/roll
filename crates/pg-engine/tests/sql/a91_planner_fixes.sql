-- Round 22, agent 91: planner fixes found by client suites and contrib tests.
-- EF Core (SelectMany_with_predicate_and_DefaultIfEmpty_...): a LATERAL subquery pulled up
-- whose FROM subquery's projection reorders columns; the WHERE's clause over the lateral
-- reference that was pushed into the scan under that projection becomes a join clause of
-- the query (it used to stay in the scan: "outer reference without an outer row").
CREATE TABLE a91_l (id int PRIMARY KEY, name text, dt timestamp NOT NULL, l2name text, req int);
INSERT INTO a91_l SELECT i, 'L1 ' || i, '2001-01-01', 'L2 ' || lpad(i::text, 2, '0'), i FROM generate_series(1, 10) i;
SELECT l.id, s.id0
FROM a91_l AS l
LEFT JOIN LATERAL (
    SELECT l2.id, l0.id AS id0
    FROM a91_l AS l0
    LEFT JOIN (
        SELECT l1.id, l1.req, l1.l2name FROM a91_l AS l1 WHERE l1.req IS NOT NULL
    ) AS l2 ON l0.id = CASE WHEN l2.req IS NOT NULL THEN l2.id END
    WHERE l2.req IS NOT NULL AND (l2.req = l.id * 2 OR length(l2.l2name)::int = l2.id)
) AS s ON TRUE
ORDER BY 1, 2;
EXPLAIN (COSTS OFF)
SELECT l.id, s.id0
FROM a91_l AS l
LEFT JOIN LATERAL (
    SELECT l2.id, l0.id AS id0
    FROM a91_l AS l0
    LEFT JOIN (
        SELECT l1.id, l1.req, l1.l2name FROM a91_l AS l1 WHERE l1.req IS NOT NULL
    ) AS l2 ON l0.id = CASE WHEN l2.req IS NOT NULL THEN l2.id END
    WHERE l2.req IS NOT NULL AND (l2.req = l.id * 2 OR length(l2.l2name)::int = l2.id)
) AS s ON TRUE
ORDER BY 1, 2;
DROP TABLE a91_l;
-- pg_overexplain's partitioned self-join with sequential scans disabled: each partition's
-- cheapest path counts its disabled nodes first, so the outer Append reads the partitions'
-- full index scans (made for the merge join's order) and the inner one their parameterized
-- index scans.
CREATE TABLE a91_veg (id serial, name text, genus text) PARTITION BY LIST (genus);
CREATE TABLE a91_daucus PARTITION OF a91_veg FOR VALUES IN ('daucus');
CREATE TABLE a91_brassica PARTITION OF a91_veg FOR VALUES IN ('brassica');
INSERT INTO a91_veg (name, genus)
	VALUES ('carrot', 'daucus'), ('bok choy', 'brassica'),
		   ('brocooli', 'brassica'), ('cauliflower', 'brassica'),
		   ('cabbage', 'brassica'), ('kohlrabi', 'brassica'),
		   ('rutabaga', 'brassica'), ('turnip', 'brassica');
CREATE INDEX ON a91_veg (id);
ANALYZE a91_veg;
SET enable_hashjoin = false;
SET enable_material = false;
SET enable_mergejoin = false;
SET enable_seqscan = false;
EXPLAIN (COSTS OFF) SELECT * FROM a91_veg v1, a91_veg v2 WHERE v1.id = v2.id;
SELECT v1.name, v2.genus FROM a91_veg v1, a91_veg v2 WHERE v1.id = v2.id ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT * FROM a91_veg v1;
RESET ALL;
DROP TABLE a91_veg;
