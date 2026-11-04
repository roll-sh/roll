# Actual PG18.6 interleavings: bitmap candidates must be rechecked after a lock finds a
# changed row version, including BitmapOr and non-index filter columns.
setup
{
  CREATE TABLE rb (id int, v int, w int);
  INSERT INTO rb SELECT i, i % 100, i % 200 FROM generate_series(1,1000) i;
  CREATE INDEX rb_v ON rb(v);
  CREATE INDEX rb_w ON rb(w);
  ANALYZE rb;
}
teardown { DROP TABLE rb; }

session s1
step begin { BEGIN; }
step change { UPDATE rb SET v=99, w=99 WHERE id=1; }
step qualifying { UPDATE rb SET v=101, w=1 WHERE id=1; }
step filteronly { UPDATE rb SET id=2 WHERE id=1; }
step commit { COMMIT; }

session s2
setup { SET enable_seqscan=off; SET enable_indexscan=off; }
step planlock { EXPLAIN (COSTS OFF) SELECT id FROM rb WHERE v=1 AND id<5 FOR UPDATE; }
step planor { EXPLAIN (COSTS OFF) SELECT id FROM rb WHERE (v=1 OR w=1) AND id<5 FOR UPDATE; }
step planupdate { EXPLAIN (COSTS OFF) UPDATE rb SET id=id+1000 WHERE v=1 AND id<5 RETURNING id; }
step lock { SELECT id FROM rb WHERE v=1 AND id<5 FOR UPDATE; }
step lockor { SELECT id FROM rb WHERE (v=1 OR w=1) AND id<5 FOR UPDATE; }
step update { UPDATE rb SET id=id+1000 WHERE v=1 AND id<5 RETURNING id; }
step plainor { SELECT id FROM rb WHERE (v=1 OR w=1) AND id<5 ORDER BY id; }
step filterlock { SELECT id FROM rb WHERE v=1 AND id<5 FOR UPDATE; }
step final { SELECT id,v,w FROM rb WHERE id IN (1,2,1001,1002) ORDER BY id; }

permutation planlock begin change lock commit final
permutation planor plainor begin change lockor commit plainor final
permutation planupdate begin change update commit final

# Still qualifies through the other OR arm after an indexed-column update.
permutation planor begin qualifying lockor commit final
# Index key is unchanged; the row returned must contain the new filter-only value.
permutation planlock begin filteronly filterlock commit final
# A qualifying UPDATE must act on the new version, not drop the candidate.
permutation planupdate begin filteronly update commit final
