-- MIN / MAX read through an index (the minmax InitPlan) keep the aggregate's own order:
-- an enum's declared order, a non-bytewise collation's order.
CREATE TYPE gsw_rainbow AS ENUM ('red', 'orange', 'yellow', 'green', 'blue', 'purple');
CREATE TABLE gsw_enumtest (col gsw_rainbow);
INSERT INTO gsw_enumtest VALUES ('red'), ('orange'), ('yellow'), ('green'), ('blue'), ('purple');
CREATE UNIQUE INDEX gsw_enumtest_btree ON gsw_enumtest USING btree (col);
SET enable_seqscan = off;
SET enable_bitmapscan = off;
SELECT min(col) FROM gsw_enumtest;
SELECT max(col) FROM gsw_enumtest;
SELECT min(col), max(col) FROM gsw_enumtest;
SELECT max(col) FROM gsw_enumtest WHERE col < 'green';
CREATE TABLE gsw_icu (t text COLLATE "en-x-icu");
INSERT INTO gsw_icu VALUES ('a'), ('B'), ('c'), ('D');
CREATE INDEX ON gsw_icu (t);
SELECT min(t), max(t) FROM gsw_icu;
SELECT min(t) FROM gsw_icu;
SELECT max(t) FROM gsw_icu;
SELECT min(t COLLATE "C") FROM gsw_icu;
RESET enable_seqscan;
RESET enable_bitmapscan;
