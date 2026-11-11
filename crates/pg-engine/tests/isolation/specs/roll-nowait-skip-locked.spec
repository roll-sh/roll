# NOWAIT and SKIP LOCKED for every row lock strength held and requested, job-queue patterns,
# and NOWAIT on relation locks.

setup
{
  CREATE TABLE r_jobs (id int PRIMARY KEY, state text NOT NULL DEFAULT 'new', worker text);
  INSERT INTO r_jobs SELECT g, 'new' FROM generate_series(1, 6) g;
}

teardown
{
  DROP TABLE r_jobs;
}

session h
setup           { BEGIN; }
step hupd       { SELECT id FROM r_jobs WHERE id IN (1, 2) FOR UPDATE; }
step hnokey     { SELECT id FROM r_jobs WHERE id IN (1, 2) FOR NO KEY UPDATE; }
step hshare     { SELECT id FROM r_jobs WHERE id IN (1, 2) FOR SHARE; }
step hkeyshare  { SELECT id FROM r_jobs WHERE id IN (1, 2) FOR KEY SHARE; }
step hupdate    { UPDATE r_jobs SET state = 'held' WHERE id IN (1, 2); }
step hupdkey    { UPDATE r_jobs SET id = id + 100 WHERE id = 1; }
step hdelete    { DELETE FROM r_jobs WHERE id = 1; }
step hlocktab   { LOCK TABLE r_jobs IN SHARE MODE; }
step hcommit    { COMMIT; }

session w
setup           { BEGIN; }
step wupdnw     { SELECT id FROM r_jobs WHERE id <= 3 ORDER BY id FOR UPDATE NOWAIT; }
step wnokeynw   { SELECT id FROM r_jobs WHERE id <= 3 ORDER BY id FOR NO KEY UPDATE NOWAIT; }
step wsharenw   { SELECT id FROM r_jobs WHERE id <= 3 ORDER BY id FOR SHARE NOWAIT; }
step wkeysharenw { SELECT id FROM r_jobs WHERE id <= 3 ORDER BY id FOR KEY SHARE NOWAIT; }
step wupdsl     { SELECT id FROM r_jobs WHERE id <= 3 ORDER BY id FOR UPDATE SKIP LOCKED; }
step wnokeysl   { SELECT id FROM r_jobs WHERE id <= 3 ORDER BY id FOR NO KEY UPDATE SKIP LOCKED; }
step wsharesl   { SELECT id FROM r_jobs WHERE id <= 3 ORDER BY id FOR SHARE SKIP LOCKED; }
step wkeysharesl { SELECT id FROM r_jobs WHERE id <= 3 ORDER BY id FOR KEY SHARE SKIP LOCKED; }
step wqueue     { UPDATE r_jobs SET state = 'taken', worker = 'w'
                  WHERE id = (SELECT id FROM r_jobs WHERE state = 'new' ORDER BY id
                              LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id, state; }
step wqueue2    { WITH next AS (SELECT id FROM r_jobs WHERE state = 'new' ORDER BY id LIMIT 2
                                FOR UPDATE SKIP LOCKED)
                  DELETE FROM r_jobs j USING next WHERE j.id = next.id RETURNING j.id; }
step wlimit     { SELECT id FROM r_jobs ORDER BY id LIMIT 2 FOR UPDATE SKIP LOCKED; }
step wlocktabnw { LOCK TABLE r_jobs IN ROW EXCLUSIVE MODE NOWAIT; }
step wupdatenw  { UPDATE r_jobs SET state = 'x' WHERE id = 3; }
step wcommit    { COMMIT; }

permutation hupd wupdnw wcommit hcommit
permutation hupd wkeysharenw wcommit hcommit
permutation hupd wupdsl wnokeysl wsharesl wkeysharesl wcommit hcommit
permutation hnokey wupdnw wcommit hcommit
permutation hnokey wnokeynw wcommit hcommit
permutation hnokey wsharenw wcommit hcommit
permutation hnokey wkeysharenw wcommit hcommit
permutation hnokey wupdsl wnokeysl wsharesl wkeysharesl wcommit hcommit
permutation hshare wupdnw wcommit hcommit
permutation hshare wnokeynw wcommit hcommit
permutation hshare wsharenw wkeysharenw wcommit hcommit
permutation hshare wupdsl wnokeysl wsharesl wkeysharesl wcommit hcommit
permutation hkeyshare wupdnw wcommit hcommit
permutation hkeyshare wnokeynw wsharenw wkeysharenw wcommit hcommit
permutation hkeyshare wupdsl wnokeysl wsharesl wkeysharesl wcommit hcommit
permutation hupdate wupdnw wcommit hcommit
permutation hupdate wkeysharenw wcommit hcommit
permutation hupdate wupdsl wkeysharesl wcommit hcommit
permutation hupdkey wkeysharenw wcommit hcommit
permutation hupdkey wkeysharesl wcommit hcommit
permutation hdelete wupdsl wkeysharenw wcommit hcommit
permutation hupd wqueue wqueue2 wcommit hcommit
permutation hupd wlimit wcommit hcommit
permutation hlocktab wlocktabnw wcommit hcommit
permutation hlocktab wupdatenw hcommit wcommit
