# A READ COMMITTED statement that waited for its relation's lock: a query takes a new snapshot
# once it holds the lock when a transaction ended meanwhile (an abort too: the backend's xmin
# moves on), and sees the commit it waited for; SQL EXECUTE keeps the snapshot it took before
# the plan's locks.
setup
{
  CREATE TABLE ss_l (i int);
  INSERT INTO ss_l VALUES (1);
  CREATE TABLE ss_t (i int);
}
teardown
{
  DROP TABLE ss_l, ss_t;
}
session o
step adv { SELECT pg_advisory_lock(1); }
step unadv { SELECT pg_advisory_unlock(1); }
step remember { SELECT set_config('ss.x', backend_xid::text, false) IS NOT NULL AS remembered FROM pg_stat_activity WHERE application_name = 'ss_w'; }
step xmin_writers { SELECT backend_xmin::text = current_setting('ss.x') AS xmin_writers FROM pg_stat_activity WHERE application_name = 'ss_r'; }
session w
setup { SET application_name = 'ss_w'; }
step wlock { BEGIN; INSERT INTO ss_t VALUES (0); LOCK TABLE ss_l IN ACCESS EXCLUSIVE MODE; }
step wadd { BEGIN; INSERT INTO ss_l VALUES (2); LOCK TABLE ss_l IN ACCESS EXCLUSIVE MODE; }
step wabort { ROLLBACK; }
step wcommit { COMMIT; }
session r
setup { SET application_name = 'ss_r'; PREPARE ss_q AS SELECT pg_advisory_xact_lock(1) FROM ss_l; PREPARE ss_c AS SELECT count(*) FROM ss_l; }
step rsel { SELECT pg_advisory_xact_lock(1) FROM ss_l; }
step rexec { EXECUTE ss_q; }
step rcount { SELECT count(*) FROM ss_l; }
step rexecc { EXECUTE ss_c; }
step rbegin { BEGIN; }
step rcommit { COMMIT; }

permutation adv wlock remember rsel xmin_writers wabort xmin_writers unadv
permutation adv wlock remember rsel xmin_writers wcommit xmin_writers unadv
permutation adv wlock remember rexec xmin_writers wabort xmin_writers unadv
permutation wadd rcount wcommit
permutation wadd rexecc wcommit
permutation rbegin wadd rexecc wcommit rcount rcommit
