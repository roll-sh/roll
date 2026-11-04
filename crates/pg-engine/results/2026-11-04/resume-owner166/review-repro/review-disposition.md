Catalog166 review response for source 256e4d049d4f82db1166c08437bab736a0d0a3ee

The lazy catalog snapshot regression is confirmed and corrected. A broad, quiet pg_class
FULL JOIN and a pg_attribute FULL JOIN expose an inner volatile function's DDL on
0cd26bfd. PostgreSQL 18.6 hides that DDL until the next statement. The new golden output
comes from PostgreSQL 18.6. The retained golden-before-fix.log and golden-before-fix.actual
show both incorrect results and exit 101. LazyRowsNode now captures its opened Arc<Catalog>.
Deferred generation, count, and key restriction use a scoped ExecCtx when catalog identity
changes. Predicate execution uses the caller's context, and the transaction keeps its DDL.

The runtime current_setting bad-OID evaluation order differs from PostgreSQL. The retained
5c312235 baseline binary reproduces the mismatch, so this batch leaves it unchanged. The
baseline binary's source provenance is supported by retained records, not newly sealed.
The parse-time regnamespace cast test does not prove runtime error order. Failed runtime
probes remain available, and no passing claim relies on that parse-time case.

Broad relkind selection builds keys before the quarter-of-catalog and WHERE_MAX = 8192
checks. Per-relation generation and composite types still need the bounded 10,000-table
latency, CPU, and footprint comparison. That gate is HELD. No performance retry occurred.

In debug builds, class_keys_checked uses generate(...).ok()? for its full-generator oracle.
If full generation returned an error, debug would fall back while release would keep keys.
The current pg_class generate_raw body has no reachable Result error branch. Future
fallibility remains a limitation. This batch makes no speculative change to that path.

SysTable::all contains 213 entries with maximum OID 13673 and no OID at or above 16384.
The flagged synthetic high-OID path is unreachable with the current inventory. That finding
does not establish behavior for a future high-OID entry.

The audit covered relations_of, pick, named lookup ownership, read_all, materialized-view
storage, index owners, class_position, and rel_oid_edits fallback. Narrow keys and
per-relation generation use the same relation owners. Moved OIDs disable optimized paths.
The LazyRowsNode filter comment is corrected. HANDOFF and BENCH describe current APIs and
LazyRowsNode's replacement of CountedRowsNode without copying unaccepted historical rates.

All six current-source bounded gate steps passed. Debug and release each run the three
a166 files in process, over the wire, and in shuffled order. Other passing counts are
catalog format 4, catalog storage 6, kept plans 7, and catalog units 31. Build and Clippy
return zero. Clippy has the same 14 diagnostic locations as the prior retained log.
The gate checks command exits, Rust summaries, golden file counts, and before/after source
hashes. Parent fresh review, combined service verification, and performance/footprint
acceptance remain pending. Earlier failed TAP and E2E runs remain failed.
