-- Round 17 (agent 37): CREATE INDEX over rows whose keys are bigger than a btree page never
-- finished (the size model's inner levels didn't shrink; PostgreSQL compresses such index
-- tuples). Found restoring the regression database's dump (compression's cmdata).
CREATE TABLE a37_cm (f1 text COMPRESSION pglz);
INSERT INTO a37_cm VALUES (repeat('1234567890', 1000));
ALTER TABLE a37_cm ALTER COLUMN f1 SET COMPRESSION lz4;
INSERT INTO a37_cm VALUES (repeat('123456789', 4004));
INSERT INTO a37_cm SELECT repeat(md5(i::text), 400) FROM generate_series(1, 20) i;
CREATE INDEX a37_cm_idx ON a37_cm (f1);
SELECT relname, relpages, reltuples FROM pg_class WHERE relname = 'a37_cm_idx';
SET enable_seqscan = off;
SELECT count(*) FROM a37_cm WHERE f1 = repeat('123456789', 4004);
RESET enable_seqscan;
DROP TABLE a37_cm;
