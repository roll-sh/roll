# UPDATEs of different rows of one heap page by transactions running at once, interleaved:
# each new version's place (its ctid: heap-only on the old version's page while the page has
# room, else another page), an aborted writer's versions holding their space until pruning, a
# row updated again by its writer and after an aborted update, and the file's size, as
# PostgreSQL's heap has them for this order of writes, commits and aborts. (The engine's size
# model lets such writers change the model at once: `exec::sizing::modellock`.)
setup
{
  CREATE TABLE hc (id int PRIMARY KEY, v int NOT NULL, pad text) WITH (fillfactor = 70);
  INSERT INTO hc SELECT g, 0, repeat('x', 150) FROM generate_series(1, 40) g;
}
teardown
{
  DROP TABLE hc;
}
session s1
setup { DEALLOCATE ALL; PREPARE u1(int) AS UPDATE hc SET v = v + 1 WHERE id = $1; }
step s1b { BEGIN; }
step s1u1 { EXECUTE u1(1); }
step s1u4 { EXECUTE u1(4); }
step s1u1again { EXECUTE u1(1); }
step s1c { COMMIT; }
session s2
step s2b { BEGIN; }
step s2u18 { UPDATE hc SET v = v + 1 WHERE id = 18; }
step s2u2 { UPDATE hc SET v = v + 1 WHERE id = 2; }
step s2u5 { UPDATE hc SET v = v + 1 WHERE id = 5; }
step s2a { ROLLBACK; }
session s3
setup { DEALLOCATE ALL; PREPARE u3(int) AS UPDATE hc SET v = v + 1 WHERE id = $1; }
step s3b { BEGIN; }
step s3u3 { EXECUTE u3(3); }
step s3u6 { EXECUTE u3(6); }
step s3u2 { EXECUTE u3(2); }
step s3many { EXECUTE u3(7); EXECUTE u3(8); EXECUTE u3(9); EXECUTE u3(10); EXECUTE u3(11); EXECUTE u3(12); EXECUTE u3(13); EXECUTE u3(14); }
step s3big { UPDATE hc SET pad = repeat('z', 900) WHERE id IN (15, 16, 17); }
step s3c { COMMIT; }
session obs
step o { SELECT id, v, ctid FROM hc ORDER BY id; }
step osize { SELECT pg_relation_size('hc') AS size; }

permutation s1b s2b s3b s1u1 s2u2 s3u3 s1u4 s2u5 s2a s3u6 s1u1again s1c s3u2 s3many s3c o osize
permutation s1b s2b s3b s3u3 s1u1 s2u2 s3u6 s1u4 s2u5 s3many s1u1again s2a s3u2 s1c s3c o osize
permutation s1b s2b s3b s1u1 s3u3 s3many s3big s2u2 s1u4 s2u18 s1u1again s3c s2u5 s1c s2a o osize
permutation s1b s3b s1u1 s3u3 s3many s1u4 s1u1again s3u6 s1c s3c s2b s2u2 s2u5 s2a o osize
