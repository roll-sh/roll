-- System columns under an indirection, and the commit timestamp functions with
-- track_commit_timestamp off (round 21, agent 108).
CREATE TABLE a108_sci (id int, r int4range);
INSERT INTO a108_sci VALUES (1, '[1,5)');
SELECT (ROW(xmin, id)).f2 FROM a108_sci;
SELECT (ROW(ctid, id, xmax)).f3 FROM a108_sci;
SELECT (pg_xact_commit_timestamp_origin(xmin)).roident FROM a108_sci;
SELECT pg_xact_commit_timestamp(xmin) FROM a108_sci;
SELECT pg_xact_commit_timestamp('0'::xid);
SELECT pg_xact_commit_timestamp('1'::xid), pg_xact_commit_timestamp('2'::xid);
SELECT * FROM pg_xact_commit_timestamp_origin('2'::xid);
SELECT * FROM pg_last_committed_xact();
SHOW track_commit_timestamp;
DROP TABLE a108_sci;
