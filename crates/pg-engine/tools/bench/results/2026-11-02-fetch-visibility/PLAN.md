# Defer unused fetch visibility setup

This is an executor change, separate from both rejected Gather candidates. It
keeps every Gather and planner path intact. No cache or cross-row state is added.

`fetched(table,key,last,ctx)` currently clones the database Xids Arc and builds
`Vis` before taking the model read lock. Unmodelled heaps and all-visible heaps
then return without using either value. The baseline fixture observation proves
1,471/1,471 pages are all-visible before and after the checked queries. Separate
baseline CPU samples show fetched ancestry in about 4.72% of range CPU and 0.22%
of point CPU. These are overlapping sampled bounds, not expected gains.

Keep the same function and its two existing model-lock scopes. Declare Xids and
Vis outside the first scope, initialize both after its missing/all-visible exits,
and retain the same tuple locate, pruning predicate, second locate under write
lock, and prune. Returning early still checks the live heap under its read lock
on every call, including after concurrent writes. There is no per-scan promise
that pages remain all-visible.

The critical fact is that skipped Xids/catalog/horizon setup performs no required
mutation other than an optional detached-catalog cache fill, or dependency recording, and its lock acquisition can safely occur
under a model read lock. ExecCtx::catalog clones the statement catalog view;
Database::xids holds its own read lock only while cloning an Arc; its only writer
is set_xids during Engine database opening and calls no model code under that
lock. Vis construction reads Xids' private snapshot state. That module has no
model dependency. Existing scanned/scanned_pages already construct Vis under a
model write lock. No Xids-state guard escapes its accessor.

Freshness on the non-all-visible path remains authoritative. The horizon may be
observed later, after the read lock is acquired, as other existing pruning code
does. SQL visibility stays owned by the transaction's snapshot, not this advisory
pruning horizon. Do not retain or reuse it across rows.

Verify with the wire pruning-horizon test, plus a public ordinary Index Scan test
that moves a table from vacuumed/all-visible to updated/non-all-visible while a
repeatable-read session retains the old row, then back after commit and VACUUM.
Both actual row results and visibility-map transitions must match. Run full
library/all-target Clippy before release, independent source review before timing,
and the full acceptance gates only if a prospectively frozen trial passes.
