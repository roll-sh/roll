- **Catalog round 10, current batch** (agent166): `system::class_keys`, `ClassSel` and
  `ClassKeep` read relation identity/kind/name from the relation maps. Statistics views
  use that directory through `stat_views::rows_of`/`StatSel`, including the nine native
  pg_statio views; leading relation/index OID bounds and name bounds narrow generation.
  `stat_views::row_estimate` supplies planner counts without generating the view.
  `system::class_rows_where` and `attribute_rows_of` generate selected relations for
  `LazyRowsNode` (which replaced `CountedRowsNode` and retains its count shortcut).
  The node captures its catalog when opened. If nested volatile DDL changes the
  transaction catalog before first next/count/restrict, deferred generation uses the
  captured version through scoped `ExecCtx::reading_catalog`; the transaction keeps its
  DDL and the next statement sees it. Golden `a166_lazy_snapshot`, recorded from
  PostgreSQL18.6, covers both pg_class and pg_attribute under inner-first FULL JOIN.
  `a166_class_where` and `a166_statio_views` cover the other batch behavior.
  Moved relation OIDs disable directory/subset generation and use the full rows.
  Narrow generation remains bounded by a quarter of rows and WHERE_MAX8192; keys for
  broad relkind filters are built before those limits, so 10k-table+composite latency,
  CPU and footprint acceptance is HELD. No round10 performance claim is accepted.
  Runtime current_setting bad-OID evaluation order differs from PostgreSQL even in the
  retained shared baseline; that mismatch is recorded separately and remains open.
