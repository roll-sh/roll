-- COPY FROM ... ON_ERROR ignore, LOG_VERBOSITY verbose: the rows skipped before a record
-- that fails the COPY still give their notices (difftest seeds 5031, 5052).
CREATE TABLE a104_cn (id int PRIMARY KEY, a int, b text);
COPY a104_cn (id, a, b) FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY verbose);
37	\N	x
26		tab\tx
1
\.
COPY a104_cn (id, a, b) FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY verbose);
37	\N	x
26	q	y
5	6	z
\.
SELECT * FROM a104_cn ORDER BY id;
DROP TABLE a104_cn;
