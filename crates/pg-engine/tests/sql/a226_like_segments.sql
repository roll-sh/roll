-- LIKE patterns of literals and % only, matched by searching their literal runs
-- (LikePattern's segments): anchored heads and tails that would overlap, runs repeated,
-- empty runs, multibyte characters, escaped % and _, NOT LIKE and NULLs, through a parallel
-- scan's batch filter and the row path.
CREATE TABLE a226_l (i int, s text, v varchar(20)) WITH (autovacuum_enabled = false);
INSERT INTO a226_l SELECT g, (ARRAY['ab', 'abb', 'aab', 'b', '', 'abab', 'héllo wörld', 'special requests', 'x%y', 'x_y', 'xy', 'a', NULL])[1 + g % 13],
  (ARRAY['ab', 'ba', 'éé', 'é', 'aé', NULL])[1 + g % 6] FROM generate_series(1, 120000) g;
VACUUM ANALYZE a226_l;
SELECT s, count(*) FROM a226_l WHERE s LIKE 'ab%b' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE 'a%a' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE '%b%b%' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE '%%' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE '%' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE '%ö%' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE 'h%ö%d' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s NOT LIKE '%special%requests%' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE 'x\%%' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE '%\_y' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE 'x#%y' ESCAPE '#' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE 'abab%ab' GROUP BY s ORDER BY s;
SELECT s, count(*) FROM a226_l WHERE s LIKE '%ab%ab' GROUP BY s ORDER BY s;
SELECT v, count(*) FROM a226_l WHERE v LIKE '%é' GROUP BY v ORDER BY v;
SELECT v, count(*) FROM a226_l WHERE v LIKE 'é%é' GROUP BY v ORDER BY v;
SELECT v, count(*) FROM a226_l WHERE v NOT LIKE '%a%' GROUP BY v ORDER BY v;
SELECT count(*) FILTER (WHERE s LIKE '%b'), count(*) FILTER (WHERE s LIKE 'a%'), count(*) FILTER (WHERE s LIKE '') FROM a226_l;
DROP TABLE a226_l;
