# LISTEN / NOTIFY: delivery at commit, in order, duplicates folded within a transaction,
# payloads, rollbacks, listeners in a transaction, LISTEN / UNLISTEN taking effect at commit,
# self-notification, pg_notify.

session l
step llisten    { LISTEN c1; LISTEN c2; }
step llisten3   { LISTEN c3; }
step lbegin     { BEGIN; }
step lbeginl    { BEGIN; LISTEN c3; }
step lselect    { SELECT 1 AS x; }
step lcommit    { COMMIT; }
step lrollback  { ROLLBACK; }
step lunlisten  { UNLISTEN c1; }
step lunlistenall { UNLISTEN *; }
step lself      { NOTIFY c1, 'self'; }
step lchannels  { SELECT pg_listening_channels() ORDER BY 1; }

session n
step nnotify    { NOTIFY c1, 'a'; }
step nbegin     { BEGIN; }
step nmany      { NOTIFY c1, 'x'; NOTIFY c2, 'y'; NOTIFY c1, 'x'; NOTIFY c1, 'z'; NOTIFY c1, 'x';
                  SELECT pg_notify('c2', 'y'); SELECT pg_notify('c2', NULL); NOTIFY c2; }
step nnopay     { NOTIFY c1; NOTIFY c1, ''; }
step nc3        { NOTIFY c3, 'three'; }
step nsave      { SAVEPOINT s; NOTIFY c1, 'in-sp'; ROLLBACK TO s; NOTIFY c1, 'after-sp'; }
step ncommit    { COMMIT; }
step nrollback  { ROLLBACK; }
step nbad       { SELECT pg_notify('', 'x'); }
step nlong      { SELECT pg_notify('c1', repeat('x', 8000)); }

# delivered at commit, in order, duplicates folded
permutation llisten nbegin nmany lselect ncommit lselect lunlistenall
# a rollback sends nothing; a savepoint rolled back takes its NOTIFY with it
permutation llisten nbegin nnotify nrollback lselect nbegin nsave ncommit lselect lunlistenall
# empty and missing payloads
permutation llisten nnopay lselect lunlistenall
# a listener in a transaction gets notifications only once it ends
permutation llisten lbegin nnotify lselect lcommit lunlistenall
# LISTEN in a transaction takes effect at commit; rolled back, never
permutation lbeginl nc3 lselect lcommit nc3 lselect lunlistenall
permutation lbeginl lrollback nc3 lselect lchannels lunlistenall
# UNLISTEN stops delivery; self-notification arrives too
permutation llisten lunlisten nnotify lself lchannels lunlistenall
# errors
permutation nbad nlong
