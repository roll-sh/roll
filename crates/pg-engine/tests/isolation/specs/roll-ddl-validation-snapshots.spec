# What DDL reads besides index builds and rewrites (agent 310's audit). In a REPEATABLE READ
# transaction, ADD CONSTRAINT CHECK / FOREIGN KEY, SET NOT NULL, VALIDATE CONSTRAINT and ALTER
# DOMAIN ADD CONSTRAINT check the newest committed rows (GetLatestSnapshot), so a violating row
# committed after the transaction's snapshot fails them: they committed constraints the table
# violated. After a lock wait, CREATE TABLE AS, SELECT INTO and CREATE MATERIALIZED VIEW read
# what the transaction they waited for committed (their query's snapshot comes after its
# locks); ANALYZE reads what committed while it waited for its SHARE UPDATE EXCLUSIVE lock;
# REFRESH MATERIALIZED VIEW reads its statement's snapshot, taken before waiting for another
# REFRESH of the view, and doesn't fail on that REFRESH's new heap.

setup
{
  CREATE TABLE v_t (a int, b int);
  INSERT INTO v_t SELECT g, g FROM generate_series(1, 10) g;
  CREATE TABLE v_p (id int PRIMARY KEY);
  INSERT INTO v_p SELECT generate_series(1, 10);
  CREATE DOMAIN v_dom AS int;
  CREATE TABLE v_d (x v_dom);
  INSERT INTO v_d VALUES (1);
  CREATE MATERIALIZED VIEW v_m AS SELECT count(*) AS n FROM v_t;
}

teardown
{
  DROP TABLE IF EXISTS v_x, v_y;
  DROP MATERIALIZED VIEW IF EXISTS v_m, v_m2;
  DROP TABLE v_t, v_p, v_d;
  DROP DOMAIN v_dom;
}

session s1
step s1rr       { BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM v_t; }
step s1check    { ALTER TABLE v_t ADD CONSTRAINT v_pos CHECK (b > 0); }
step s1notnull  { ALTER TABLE v_t ALTER COLUMN b SET NOT NULL; }
step s1fk       { ALTER TABLE v_t ADD CONSTRAINT v_fk FOREIGN KEY (b) REFERENCES v_p; }
step s1domain   { ALTER DOMAIN v_dom ADD CONSTRAINT v_dpos CHECK (VALUE > 0); }
step s1commit   { COMMIT; }
step s1lock     { BEGIN; LOCK TABLE v_t IN ACCESS EXCLUSIVE MODE; INSERT INTO v_t VALUES (11, 11); }
step s1sue      { BEGIN; LOCK TABLE v_t IN SHARE UPDATE EXCLUSIVE MODE; }
step s1refresh  { BEGIN; REFRESH MATERIALIZED VIEW v_m; }

session s2
step s2neg      { INSERT INTO v_t VALUES (11, -1); }
step s2null     { INSERT INTO v_t VALUES (11, NULL); }
step s2orphan   { INSERT INTO v_t VALUES (11, 99); }
step s2parent   { DELETE FROM v_p WHERE id = 4; }
step s2domain   { INSERT INTO v_d VALUES (-1); }
step s2more     { INSERT INTO v_t SELECT g, g FROM generate_series(11, 500) g; }
step s2one      { INSERT INTO v_t VALUES (11, 11); }
step s2cons     {
  SELECT conname, convalidated FROM pg_constraint
   WHERE conrelid IN ('v_t'::regclass, 'v_d'::regclass) OR contypid = 'v_dom'::regtype ORDER BY 1;
  SELECT attnotnull FROM pg_attribute WHERE attrelid = 'v_t'::regclass AND attname = 'b';
}

session s3
step s3ctas     { CREATE TABLE v_x AS SELECT * FROM v_t; }
step s3into     { SELECT * INTO v_y FROM v_t; }
step s3cmv      { CREATE MATERIALIZED VIEW v_m2 AS SELECT count(*) AS n FROM v_t; }
step s3analyze  { ANALYZE v_t; }
step s3refresh  { REFRESH MATERIALIZED VIEW v_m; }
step s3copied   { SELECT (SELECT count(*) FROM v_x) AS ctas, (SELECT count(*) FROM v_y) AS into_rows; }
step s3cmvrows  { SELECT n FROM v_m2; }
step s3tuples   { SELECT reltuples FROM pg_class WHERE relname = 'v_t'; }
step s3mv       { SELECT n FROM v_m; }

# Validations in REPEATABLE READ see a violating row committed after the snapshot.
permutation s1rr s2neg s1check s1commit s2cons
permutation s1rr s2null s1notnull s1commit s2cons
permutation s1rr s2orphan s1fk s1commit s2cons
permutation s1rr s2parent s1fk s1commit s2cons
permutation s1rr s2domain s1domain s1commit s2cons
# After a lock wait.
permutation s1lock s3ctas s1commit s3into s3copied
permutation s1lock s3into s1commit s3ctas s3copied
permutation s1lock s3cmv s1commit s3cmvrows
permutation s1sue s3analyze s2more s1commit s3tuples
permutation s1refresh s3refresh s2one s1commit s3mv
