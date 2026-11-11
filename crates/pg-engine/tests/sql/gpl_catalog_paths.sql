-- System catalog scans planned as a table's: an index read in order for an ORDER BY
-- (forward or backward, index-only), row comparisons and LIKE prefixes as index
-- conditions, bitmap scans; the rows in the index's order.
EXPLAIN (COSTS OFF)
SELECT proname, proargtypes, pronamespace FROM pg_proc
 WHERE (proname, pronamespace) > ('abs', 0)
 ORDER BY proname, proargtypes, pronamespace LIMIT 1;
SELECT proname, proargtypes, pronamespace FROM pg_proc
 WHERE (proname, pronamespace) > ('abs', 0)
 ORDER BY proname, proargtypes, pronamespace LIMIT 1;
EXPLAIN (COSTS OFF)
SELECT proname, proargtypes, pronamespace FROM pg_proc
 WHERE (proname, pronamespace) < ('abs', 1000000)
 ORDER BY proname DESC, proargtypes DESC, pronamespace DESC LIMIT 1;
SELECT proname, proargtypes, pronamespace FROM pg_proc
 WHERE (proname, pronamespace) < ('abs', 1000000)
 ORDER BY proname DESC, proargtypes DESC, pronamespace DESC LIMIT 1;
EXPLAIN (COSTS OFF)
SELECT proname, proargtypes, pronamespace FROM pg_proc
 WHERE (proname, proargtypes) >= ('abs', NULL) AND proname <= 'abs'
 ORDER BY proname, proargtypes, pronamespace;
SELECT proname, proargtypes, pronamespace FROM pg_proc
 WHERE proname = 'abs' AND pronamespace IN (11, 2)
 ORDER BY proname, proargtypes, pronamespace;
SET enable_seqscan = off;
SET enable_bitmapscan = off;
EXPLAIN (COSTS OFF) SELECT proname FROM pg_proc WHERE proname LIKE 'RI\_FKey%del' ORDER BY 1;
SELECT proname FROM pg_proc WHERE proname LIKE 'RI\_FKey%del' ORDER BY 1;
SET enable_indexscan = off;
SET enable_bitmapscan = on;
EXPLAIN (COSTS OFF) SELECT proname FROM pg_proc WHERE proname LIKE 'RI\_FKey%del' ORDER BY 1;
SELECT proname FROM pg_proc WHERE proname LIKE 'RI\_FKey%del' ORDER BY 1;
EXPLAIN (COSTS OFF) SELECT proname FROM pg_proc WHERE proname ILIKE 'ri%foo' ORDER BY 1;
RESET enable_seqscan;
RESET enable_indexscan;
RESET enable_bitmapscan;
SELECT relname FROM pg_class WHERE relname LIKE 'pg\_class%' ORDER BY relname DESC;
SELECT typname FROM pg_type WHERE oid < 30 ORDER BY oid;
