MYDB='(SELECT oid FROM pg_database WHERE datname = current_database())'
# Scenarios of e2e.sh (sourced): each sc_<name> builds its databases, runs its workload on
# both publishers, waits for the subscribers and checks them.

# 016 / 018 / 019: streamed transactions with rolled-back subtransactions (nested, released
# into a parent that rolls back, a PL/pgSQL exception block, an aborted transaction), DDL in
# a rolled-back subtransaction; streaming = on and parallel, logical_decoding_work_mem 64kB.
stream_abort_run() {
  d=$1; mode=$2
  fresh $d
  on_pubs $d "CREATE TABLE t (a int PRIMARY KEY, b bytea, c text); CREATE TABLE t2 (a int); CREATE PUBLICATION p FOR TABLE t, t2; INSERT INTO t VALUES (1, 'foo'), (2, 'bar')" >/dev/null
  on_subs $d "CREATE TABLE t (a int PRIMARY KEY, b bytea, c text, d int DEFAULT 7); CREATE TABLE t2 (a int)" >/dev/null
  subscribe $d $d p "streaming = $mode"
  caught_up $d
  on_pubs $d "BEGIN;
    INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(3, 400) i;
    SAVEPOINT s1; INSERT INTO t SELECT i, sha256(i::text::bytea), repeat('x', 300) FROM generate_series(401, 800) i;
    SAVEPOINT s2; INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(801, 1200) i;
    SAVEPOINT s3; UPDATE t SET c = 's3' WHERE a % 3 = 0; ROLLBACK TO s2;
    INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(1201, 1500) i; ROLLBACK TO s1;
    INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(1501, 1700) i;
    SAVEPOINT s4; DELETE FROM t WHERE a % 5 = 0; SAVEPOINT s5; INSERT INTO t2 SELECT generate_series(1, 2000); RELEASE s5;
    UPDATE t SET c = 'after' WHERE a % 7 = 0; COMMIT" >/dev/null
  on_pubs $d "BEGIN; INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(2001, 2400) i;
    SAVEPOINT a; INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(2401, 2800) i;
    SAVEPOINT b; INSERT INTO t2 SELECT generate_series(1, 3000); RELEASE b; ROLLBACK TO a; COMMIT" >/dev/null
  on_pubs $d "BEGIN; INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(3001, 4000) i; SAVEPOINT a; INSERT INTO t2 SELECT generate_series(1, 2000); ROLLBACK" >/dev/null
  on_pubs $d "DO \$\$ BEGIN
      INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(4001, 4300) i;
      FOR k IN 1..5 LOOP
        BEGIN INSERT INTO t SELECT i, sha256(i::text::bytea), 'blk' FROM generate_series(5000 + k * 1000, 5000 + k * 1000 + 300) i;
          IF k % 2 = 1 THEN PERFORM 1 / 0; END IF;
        EXCEPTION WHEN division_by_zero THEN NULL; END;
      END LOOP; END \$\$" >/dev/null
  # 019: DDL in a rolled-back subtransaction of a streamed transaction.
  on_pubs $d "BEGIN; INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(20001, 20500) i;
    SAVEPOINT s1; ALTER TABLE t ADD COLUMN e int; INSERT INTO t SELECT i, sha256(i::text::bytea), 'e', i FROM generate_series(20501, 21000) i;
    ROLLBACK TO s1; INSERT INTO t SELECT i, sha256(i::text::bytea) FROM generate_series(21001, 21200) i; COMMIT" >/dev/null
  caught_up $d
  check $d "$mode rows" "$(rowsums t t2)"
  check $d "$mode errors" "SELECT apply_error_count, sync_error_count FROM pg_stat_subscription_stats WHERE subid IN (SELECT oid FROM pg_subscription WHERE subdbid = $MYDB) ORDER BY subname"
}
sc_stream_abort() { stream_abort_run sa_on on; stream_abort_run sa_par parallel; }

# 028 / 031: a row filter with a column list; UPDATEs moving rows into and out of the
# filter (sent as INSERT / DELETE), the initial copy filtered too.
sc_filter_cols() {
  d=fc; fresh $d
  on_pubs $d "CREATE TABLE t (id int PRIMARY KEY, a int, b text, c text);
    INSERT INTO t SELECT g, g % 10, 'b' || g, 'c' || g FROM generate_series(1, 200) g;
    CREATE TABLE u (id int, a int, b text); ALTER TABLE u REPLICA IDENTITY FULL; INSERT INTO u SELECT g, g, 'u' FROM generate_series(1, 50) g;
    CREATE PUBLICATION p FOR TABLE t (id, a, b) WHERE (id > 50), u WHERE (a % 2 = 0)" >/dev/null
  on_subs $d "CREATE TABLE t (id int PRIMARY KEY, a int, b text, c text DEFAULT 'local'); CREATE TABLE u (id int, a int, b text)" >/dev/null
  subscribe $d fc p "copy_data = true"
  caught_up $d
  on_pubs $d "UPDATE t SET id = id + 1000 WHERE id % 10 = 1 AND id < 50; UPDATE t SET id = id + 2000 WHERE id BETWEEN 120 AND 140; UPDATE t SET a = 0 WHERE id % 10 = 6; UPDATE t SET b = 'x' || b, c = 'nope';
    DELETE FROM t WHERE id % 13 = 0; INSERT INTO t SELECT g, g % 10, 'n', 'n' FROM generate_series(201, 260) g;
    UPDATE u SET a = a + 1 WHERE id < 20; DELETE FROM u WHERE id > 45; UPDATE u SET b = 'v' WHERE a % 2 = 0" >/dev/null
  caught_up $d
  check $d rows "$(rowsums t u)"
  check $d rel "SELECT r.srsubstate, c.relname FROM pg_subscription_rel r JOIN pg_class c ON c.oid = r.srrelid ORDER BY 2"
}

# 013: a partitioned table published through its root (publish_via_partition_root), into a
# plain table and into a table partitioned another way; TRUNCATE.
sc_via_root() {
  d=vr; fresh $d
  on_pubs $d "CREATE TABLE p (a int PRIMARY KEY, b text) PARTITION BY RANGE (a);
    CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (100); CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (100) TO (1000);
    CREATE TABLE q (a int, b text) PARTITION BY LIST (b); CREATE TABLE qa PARTITION OF q FOR VALUES IN ('a'); CREATE TABLE qb PARTITION OF q DEFAULT;
    INSERT INTO p SELECT g, 'x' FROM generate_series(1, 150) g; INSERT INTO q SELECT g, CASE WHEN g % 2 = 0 THEN 'a' ELSE 'b' END FROM generate_series(1, 40) g;
    CREATE PUBLICATION pr FOR TABLE p WITH (publish_via_partition_root = true); CREATE PUBLICATION pq FOR TABLE q WITH (publish_via_partition_root = true, publish = 'insert')" >/dev/null
  on_subs $d "CREATE TABLE p (a int PRIMARY KEY, b text) PARTITION BY HASH (a);
    CREATE TABLE ph0 PARTITION OF p FOR VALUES WITH (MODULUS 2, REMAINDER 0); CREATE TABLE ph1 PARTITION OF p FOR VALUES WITH (MODULUS 2, REMAINDER 1);
    CREATE TABLE q (a int, b text)" >/dev/null
  subscribe $d vr "pr, pq" "copy_data = true"
  caught_up $d
  on_pubs $d "UPDATE p SET a = a + 500 WHERE a % 9 = 0; UPDATE p SET b = 'y' WHERE a < 50; DELETE FROM p WHERE a % 11 = 0;
    INSERT INTO q VALUES (100, 'a'), (101, 'z'); UPDATE q SET b = 'a' WHERE a = 1; DELETE FROM q WHERE a = 2" >/dev/null
  caught_up $d
  rows_ok=1
  check $d rows "$(rowsums p q ph0 ph1)" || rows_ok=0
  check $d sync "SELECT count(*), count(DISTINCT srrelid), bool_and(srsubstate = 'r') FROM pg_subscription_rel;
    SELECT sync_error_count FROM pg_stat_subscription_stats WHERE subid IN (SELECT oid FROM pg_subscription WHERE subdbid = $MYDB)" || rows_ok=0
  on_pubs $d "TRUNCATE p" >/dev/null
  caught_up $d
  check $d truncate "$(rowsums p)"
  [ "$rows_ok" = 1 ]
}

# 021 / 023: two_phase = true, prepared transactions committed and rolled back, one
# streamed with rolled-back subtransactions.
sc_twophase() {
  d=tp; fresh $d
  on_pubs $d "CREATE TABLE t (a int PRIMARY KEY, b text); CREATE PUBLICATION p FOR TABLE t; INSERT INTO t VALUES (1, 'a')" >/dev/null
  on_subs $d "CREATE TABLE t (a int PRIMARY KEY, b text)" >/dev/null
  subscribe $d tp p "two_phase = true, streaming = on"
  caught_up $d
  # Enabling two-phase decoding happens after initial synchronization completes.
  # Feedback alone can arrive before that catalog update.
  for sp in $P_RSUB $P_ESUB; do
    for suffix in r e; do
      for _ in $(seq 120); do
        state=$(q "$sp" "${d}_$suffix" "SELECT subtwophasestate FROM pg_subscription WHERE subdbid = $MYDB")
        [ "$state" = e ] && break
        sleep 0.5
      done
      [ "$state" = e ] || { log "$d: two-phase decoding was not enabled on $sp/$suffix"; return 1; }
    done
  done
  check $d state "SELECT subtwophasestate FROM pg_subscription WHERE subdbid = $MYDB"
  on_pubs $d "BEGIN; INSERT INTO t SELECT g, 'p1' FROM generate_series(2, 50) g; PREPARE TRANSACTION 'g1'" >/dev/null
  on_pubs $d "BEGIN; INSERT INTO t SELECT g, 'p2' FROM generate_series(100, 3000) g; SAVEPOINT s; UPDATE t SET b = 'rb'; ROLLBACK TO s;
    DELETE FROM t WHERE a % 3 = 0; PREPARE TRANSACTION 'g2'" >/dev/null
  on_pubs $d "BEGIN; INSERT INTO t SELECT g, 'p3' FROM generate_series(5000, 5100) g; PREPARE TRANSACTION 'g3'" >/dev/null
  caught_up $d
  check $d prepared "SELECT count(*) FROM pg_prepared_xacts WHERE database = current_database(); $(rowsums t)"
  for x in "COMMIT PREPARED 'g1'" "COMMIT PREPARED 'g2'" "ROLLBACK PREPARED 'g3'"; do on_pubs $d "$x" >/dev/null; done
  caught_up $d
  check $d finished "SELECT count(*) FROM pg_prepared_xacts WHERE database = current_database(); $(rowsums t)"
}

# 029: disable_on_error and ALTER SUBSCRIPTION ... SKIP (the failing transaction's finish
# LSN, from the subscriber's server log).
sc_on_error() {
  d=oe; fresh $d
  on_pubs $d "CREATE TABLE t (a int PRIMARY KEY, b text); CREATE PUBLICATION p FOR TABLE t" >/dev/null
  on_subs $d "CREATE TABLE t (a int PRIMARY KEY, b text); INSERT INTO t VALUES (5, 'local')" >/dev/null
  subscribe $d oe p "disable_on_error = true"
  caught_up $d
  on_pubs $d "INSERT INTO t VALUES (1, 'one'); BEGIN; INSERT INTO t VALUES (4, 'x'), (5, 'conflict'), (6, 'y'); COMMIT; INSERT INTO t VALUES (7, 'after')" >/dev/null
  for _ in $(seq 100); do
    n=$(for p in $P_RSUB $P_ESUB; do for s in r e; do q $p ${d}_$s "SELECT subenabled FROM pg_subscription WHERE subdbid = $MYDB"; done; done | grep -c f || true)
    [ "$n" = 4 ] && break; sleep 0.3
  done
  check $d disabled "SELECT subenabled FROM pg_subscription WHERE subdbid = $MYDB; SELECT apply_error_count FROM pg_stat_subscription_stats WHERE subid IN (SELECT oid FROM pg_subscription WHERE subdbid = $MYDB); $(rowsums t)"
  # SKIP the failed transaction: its finish LSN is in the error CONTEXT.
  for x in "$P_RSUB r $A-xrsub" "$P_RSUB e $A-xrsub" "$P_ESUB r $A-xesub" "$P_ESUB e $A-xesub"; do
    set -- $x
    sub=$(q $1 ${d}_$2 "SELECT subname FROM pg_subscription WHERE subdbid = $MYDB")
    if [ "$3" = "$A-xrsub" ]; then lines=$(docker logs "$3" 2>&1); else lines=$(cat "$WORK/$3.log"); fi
    lsn=$(echo "$lines" | grep "replication origin \"pg_$(q $1 ${d}_$2 "SELECT oid FROM pg_subscription WHERE subdbid = $MYDB")\"" | grep -o 'finished at [0-9A-F/]*' | tail -1 | cut -d' ' -f3)
    q $1 ${d}_$2 "ALTER SUBSCRIPTION $sub SKIP (lsn = '$lsn'); ALTER SUBSCRIPTION $sub ENABLE" >/dev/null
  done
  caught_up $d
  check $d skipped "SELECT subenabled, subskiplsn FROM pg_subscription WHERE subdbid = $MYDB; $(rowsums t)"
}

# 026 / 035: conflicts logged and counted (insert_exists stops apply; update_missing,
# delete_missing, update_origin_differs and delete_origin_differs apply on).
sc_conflicts() {
  d=cf; fresh $d
  on_pubs $d "CREATE TABLE t (a int PRIMARY KEY, b text); INSERT INTO t SELECT g, 'p' FROM generate_series(1, 20) g; CREATE PUBLICATION p FOR TABLE t" >/dev/null
  on_subs $d "CREATE TABLE t (a int PRIMARY KEY, b text)" >/dev/null
  subscribe $d cf p "copy_data = true"
  caught_up $d
  on_subs $d "DELETE FROM t WHERE a <= 3; UPDATE t SET b = 'local' WHERE a BETWEEN 10 AND 12" >/dev/null
  on_pubs $d "UPDATE t SET b = 'u' WHERE a <= 2; DELETE FROM t WHERE a = 3; UPDATE t SET b = 'u2' WHERE a = 10; DELETE FROM t WHERE a = 11" >/dev/null
  caught_up $d
  check $d counts "SELECT apply_error_count, confl_insert_exists, confl_update_origin_differs, confl_update_exists, confl_update_missing, confl_delete_origin_differs, confl_delete_missing, confl_multiple_unique_conflicts FROM pg_stat_subscription_stats WHERE subid IN (SELECT oid FROM pg_subscription WHERE subdbid = $MYDB); $(rowsums t)"
}

# 033: run_as_owner = false (the table owner applies; its trigger sees it as current_user)
# and true.
sc_run_as_owner() {
  d=ro; fresh $d
  for p in $P_RSUB $P_ESUB; do q $p postgres "DROP ROLE IF EXISTS regress_ro_owner; CREATE ROLE regress_ro_owner" >/dev/null 2>&1 || true; done
  on_pubs $d "CREATE TABLE t (a int PRIMARY KEY); CREATE PUBLICATION p FOR TABLE t" >/dev/null
  on_subs $d "CREATE TABLE t (a int PRIMARY KEY); CREATE TABLE seen (who name, a int);
    CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS \$\$ BEGIN INSERT INTO public.seen VALUES (current_user, NEW.a); RETURN NEW; END \$\$;
    CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f(); ALTER TABLE t ENABLE ALWAYS TRIGGER tr;
    GRANT ALL ON seen TO regress_ro_owner; ALTER TABLE t OWNER TO regress_ro_owner" >/dev/null
  subscribe $d ro p "copy_data = false"
  caught_up $d
  on_pubs $d "INSERT INTO t VALUES (1), (2)" >/dev/null
  caught_up $d
  check $d owner "SELECT * FROM seen ORDER BY a"
  for x in "$P_RSUB r" "$P_RSUB e" "$P_ESUB r" "$P_ESUB e"; do set -- $x; q $1 ${d}_$2 "ALTER SUBSCRIPTION $(q $1 ${d}_$2 "SELECT subname FROM pg_subscription WHERE subdbid = $MYDB") SET (run_as_owner = true)" >/dev/null; done
  on_pubs $d "INSERT INTO t VALUES (3)" >/dev/null
  caught_up $d
  check $d as_owner "SELECT * FROM seen ORDER BY a; SELECT subrunasowner FROM pg_subscription WHERE subdbid = $MYDB"
}

# Failover slots: CREATE SUBSCRIPTION ... (failover = true) makes the publisher's slot a
# failover one; ALTER ... SET (failover) needs the subscription disabled.
sc_failover() {
  d=fo; fresh $d
  on_pubs $d "CREATE TABLE t (a int); CREATE PUBLICATION p FOR TABLE t" >/dev/null
  on_subs $d "CREATE TABLE t (a int)" >/dev/null
  subscribe $d fo p "failover = true"
  caught_up $d
  for pp in $P_RPUB $P_EPUB; do q $pp $d "SELECT slot_name, failover, two_phase FROM pg_replication_slots WHERE database = '$d' ORDER BY 1"; done | sed 's/_[re][re]|/|/' | sort > "$WORK/fo.slots"
  check $d sub "SELECT subfailover FROM pg_subscription WHERE subdbid = $MYDB"
  log "failover slots on the publishers: $(tr '\n' ' ' < "$WORK/fo.slots")"
  for pp in $P_RPUB $P_EPUB; do
    [ "$(q "$pp" "$d" "SELECT count(*) = 2 AND bool_and(failover AND NOT two_phase) FROM pg_replication_slots WHERE database = '$d' AND slot_name NOT LIKE 'pg_%_sync_%'")" = t ] || {
      log "failover slot flags differ on $pp"; return 1;
    }
  done
  for x in "$P_RSUB r" "$P_ESUB r"; do
    set -- $x
    expect_sqlstate "$1" "${d}_$2" 55000 "ALTER SUBSCRIPTION $(q "$1" "${d}_$2" "SELECT subname FROM pg_subscription WHERE subdbid = $MYDB") SET (failover = false)"
  done
}

# 030: origin = none between two nodes subscribing to each other (no loop, each row once),
# and copy_data = true with origin = none warning about rows from other origins.
sc_origin() {
  d=og; fresh $d
  # The publishers subscribe back to their subscribers' "_r" / "_e" databases.
  on_pubs $d "CREATE TABLE t (a int PRIMARY KEY, src text); CREATE PUBLICATION p FOR TABLE t" >/dev/null
  on_subs $d "CREATE TABLE t (a int PRIMARY KEY, src text); CREATE PUBLICATION pb FOR TABLE t" >/dev/null
  subscribe $d og p "origin = none, copy_data = false"
  for x in "$P_RPUB $P_RSUB r" "$P_EPUB $P_ESUB e"; do
    set -- $x
    q $1 $d "CREATE SUBSCRIPTION ogb CONNECTION 'host=127.0.0.1 port=$2 user=postgres dbname=${d}_$3' PUBLICATION pb WITH (origin = none, copy_data = false)" >/dev/null
  done
  caught_up $d
  on_pubs $d "INSERT INTO t VALUES (1, 'pub'), (2, 'pub')" >/dev/null
  on_subs $d "INSERT INTO t VALUES (10, 'sub')" >/dev/null
  caught_up $d; sleep 2
  check $d rows "$(rowsums t)"
  for x in "$P_RPUB" "$P_EPUB"; do q $x $d "SELECT '$x', count(*) FROM t"; q $x $d "ALTER SUBSCRIPTION ogb DISABLE; ALTER SUBSCRIPTION ogb SET (slot_name = NONE); DROP SUBSCRIPTION ogb" >/dev/null; done >&2
}
