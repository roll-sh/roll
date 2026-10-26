# Sequences under concurrency: nextval is never rolled back, setval is seen at once by other
# sessions, CACHE hands each session its own range, currval / lastval are per session,
# ALTER SEQUENCE and nextval wait for each other.

setup
{
  CREATE SEQUENCE r_s;
  CREATE SEQUENCE r_c CACHE 10;
  CREATE TABLE r_ids (id int DEFAULT nextval('r_s'), who text);
}

teardown
{
  DROP TABLE r_ids;
  DROP SEQUENCE r_s, r_c;
}

session s1
step s1next     { SELECT nextval('r_s'); }
step s1cnext    { SELECT nextval('r_c'); }
step s1curr     { SELECT currval('r_s'), lastval(); }
step s1currc    { SELECT currval('r_c'); }
step s1begin    { BEGIN; }
step s1ins      { INSERT INTO r_ids (who) VALUES ('s1') RETURNING id; }
step s1setval   { SELECT setval('r_s', 100); }
step s1setvalf  { SELECT setval('r_s', 200, false); }
step s1alter    { ALTER SEQUENCE r_s RESTART WITH 500; }
step s1alterinc { ALTER SEQUENCE r_s INCREMENT BY 10; }
step s1rollback { ROLLBACK; }
step s1commit   { COMMIT; }
step s1state    { SELECT last_value, is_called FROM r_s; }

session s2
step s2next     { SELECT nextval('r_s'); }
step s2cnext    { SELECT nextval('r_c'); }
step s2curr     { SELECT currval('r_s'); }
step s2ins      { INSERT INTO r_ids (who) VALUES ('s2') RETURNING id; }
step s2state    { SELECT last_value, is_called FROM r_s; }
step s2ids      { SELECT id, who FROM r_ids ORDER BY id; }

# interleaved nextval; a rollback leaves a gap
permutation s1next s2next s1begin s1ins s2ins s1rollback s2ids s1next s1curr s2curr
# setval inside a transaction is seen by others at once and survives the rollback
permutation s1begin s1setval s2state s2next s1rollback s1state s1setvalf s2next
# CACHE: each session takes its own range
permutation s1cnext s2cnext s1cnext s2cnext s1currc
# ALTER SEQUENCE in a transaction: nextval waits for it (it holds the sequence's lock)
permutation s1begin s1alter s2next s1commit s2state
permutation s1begin s1alterinc s2next s1rollback s2next
# currval before nextval in this session
permutation s2curr s1next s2curr
