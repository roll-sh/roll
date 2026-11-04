## Catalog round 10: catalog generation and snapshots (agent 166, 2026-11-04)

The batch narrows catalog and statistics-view generation and defers broad pg_class and
pg_attribute rows until first consumption. HANDOFF's "Catalog round 10" describes the APIs.
The initial deferred generation exposed inner volatile DDL to an already-open outer catalog
scan. PostgreSQL 18.6 hides the new relation or attribute until the next statement. The new
`a166_lazy_snapshot` golden records that result, and `256e4d04` retains the opened catalog.
Its bounded debug and release checks pass. Exact commands, per-command exits, source hashes,
and raw logs are in `results/2026-11-04/resume-owner166/review-repro/`.

No new performance acceptance is claimed. The original loaded-host timings remain historical
evidence. Broad relkind filters build keys before the `WHERE_MAX = 8192` and quarter-of-catalog
checks. Per-relation generation still requires a bounded comparison with 10,000 tables and
composite types, including latency, CPU, and footprint. That gate is HELD. Combined service
verification remains pending. Earlier failed pg_dump TAP and E2E outputs remain failed,
including runs whose Rust wrapper returned zero.

