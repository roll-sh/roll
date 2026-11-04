- **Catalog round 10, current batch** (agent 166): `system::class_keys`, `ClassSel`, and
  `ClassKeep` read relation identity, kind, and name from the relation maps. Statistics
  views use that directory through `stat_views::rows_of` and `StatSel`, including the nine
  native pg_statio views. Leading relation OID, index OID, and name bounds narrow the rows.
  `stat_views::row_estimate` supplies planner counts without generating the view.
  `system::class_rows_where` and `attribute_rows_of` generate selected relations for
  `LazyRowsNode`, which replaced `CountedRowsNode` and retains its count shortcut.
  The node captures its catalog when opened. If nested volatile DDL changes the
  transaction catalog before first consumption, deferred generation uses the captured
  version through `ExecCtx::reading_catalog`. The transaction keeps its DDL and the next
  statement sees it. Golden `a166_lazy_snapshot`, recorded from PostgreSQL 18.6, covers
  pg_class and pg_attribute under an inner-first FULL JOIN. Goldens `a166_class_where` and
  `a166_statio_views` cover the other batch behavior. Moved relation OIDs disable the
  directory and subset paths, which fall back to full row generation.
  On `256e4d04`, debug and release each pass all three a166 goldens in process, over the
  wire, and in shuffled order. Catalog format tests pass 4/4, catalog storage 6/6, kept
  plans 7/7, and catalog unit tests 31/31. Build and Clippy pass. Clippy has the same 14
  diagnostic locations as the prior retained log. Exact commands, logs, source hashes,
  and review dispositions are in `results/2026-11-04/resume-owner166/review-repro/`.
  Subset generation has a quarter-of-catalog limit and `WHERE_MAX = 8192`. Broad relkind
  filters build keys before those limits. The 10,000-table plus composite-type latency,
  CPU, and footprint gate remains HELD. Combined service verification remains pending.
  The runtime `current_setting` bad-OID evaluation-order mismatch also occurs in the
  retained shared baseline and remains open. No round 10 performance claim is accepted.
