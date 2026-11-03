-- ALTER COLUMN ... SET (n_distinct / n_distinct_inherited) stands for what ANALYZE's sample
-- gives (do_analyze_rel), and RESET gives the sample's back. (v's inherited estimate is left
-- out: PostgreSQL's own varies with its random sample.)
CREATE TABLE a146_nd (u int, v int, w int);
INSERT INTO a146_nd SELECT g % 3000, g, g % 7 FROM generate_series(1, 30000) g;
ALTER TABLE a146_nd ALTER COLUMN u SET (n_distinct = 50);
ALTER TABLE a146_nd ALTER COLUMN v SET (n_distinct = -0.5);
ANALYZE a146_nd;
SELECT attname, inherited, n_distinct FROM pg_stats WHERE tablename = 'a146_nd' AND NOT (attname = 'v' AND inherited) ORDER BY 1, 2;
CREATE TABLE a146_nd_c () INHERITS (a146_nd);
INSERT INTO a146_nd_c SELECT g % 10, g, 1 FROM generate_series(1, 100) g;
ALTER TABLE a146_nd ALTER COLUMN u SET (n_distinct_inherited = 7);
ANALYZE a146_nd;
SELECT attname, inherited, n_distinct FROM pg_stats WHERE tablename = 'a146_nd' AND NOT (attname = 'v' AND inherited) ORDER BY 1, 2;
ALTER TABLE a146_nd ALTER COLUMN u RESET (n_distinct);
ANALYZE a146_nd;
SELECT attname, inherited, n_distinct FROM pg_stats WHERE tablename = 'a146_nd' AND NOT (attname = 'v' AND inherited) ORDER BY 1, 2;
DROP TABLE a146_nd CASCADE;
