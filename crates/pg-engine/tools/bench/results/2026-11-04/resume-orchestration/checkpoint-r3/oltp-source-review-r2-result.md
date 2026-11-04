# OLTP165 second review (R2): `caabeb196e268f5a0fb822e30565e633344058d1`

This was a static read only. I made no edits, builds, tests, benchmarks or services, and HEAD, the index and the sources are untouched. The one extra check was a trial three-way merge, `git merge-tree 575ad4b2 63f7c0b4 caabeb19`, printed to stdout; it writes no objects.

**Verdict:** all four R1 findings look fixed in the code. I found no new HIGH problem. Two MEDIUM and three LOW issues remain. **This is not a source-ready verdict.** The full exact-source gates are still running and have not passed.

## Status of the R1 findings

| R1 finding | Status | Where in caab |
|---|---|---|
| HIGH: prepared-transaction lock leak through the shared `moved` flag | **Fixed (static read)** | `txn/twophase.rs:457` (cached slot set to `None` on detach); `txn/locks.rs:463,941,989,1887` (`moved` is now a per-owner `HashSet` under the `holds` mutex, cleared for the owner in `transfer`) |
| HIGH: cursor restart repeats observable effects; O(n²) `BACKWARD ALL` | **Fixed for the listed shapes** | `session/cursor.rs:644,1275-1311` (restart only for immutable scan/filter/project/limit/append, with no user calls and no subqueries); `:708` (at most one restart, then rows are kept) |
| MEDIUM: storage keys grow without bound while streaming | **Fixed** | `cursor.rs:736-747` (keys drained together with rows, current key kept); `:1231` (multi-reference key recorded once); `:1253-1257` (`WHERE CURRENT OF` indexing) |
| LOW: `lit_bind`/`lit_replan` rebuild the search-path Arc | **Fixed** | `session/litplan.rs:332,343` now clone the `SearchPath` Arc |

Lock-side checks that came out clean:
- **Lock order:** `transfer` takes state, then the slot list, then each slot's holds. That is the same order as `move_fast`, and `end_proc` drops the slot list before taking state. I see no deadlock.
- **Reused backend slot:** after detach, the slot's `xact` field still names the prepared transaction until the next one overwrites it. The only effect is a no-op move in `move_fast_xact`. Harmless.
- **Recovered prepared transactions:** `proc_of(id, None)` has the top bit clear, so they get no fast slot at all.
- **Session disconnect:** the transaction still holds the `Arc` of the removed slot, so its per-owner `moved` entry survives `end_proc`.
- **Catalog version reuse:** `Catalog::bump` makes versions unique across the process (`catalog/mod.rs:4662`), so a rolled-back catalog's version is never reused. The `usable_path_memo` key (version, role, database owner, Arc identity of the path) therefore can't hit a stale entry.

## Remaining and new issues

1. **MEDIUM (memory regression vs baeee87a, same as base 575ad4b2) — `cursor.rs:644,682`.**
   - With the stricter restart rule, any implicitly or explicitly scrollable cursor whose plan is not immutable now keeps every row it fetches. That covers quals using `now()`, `current_setting`, `current_user` or RLS, STABLE timestamptz casts, and Sort, FunctionScan or CTE plans.
   - Example: a psycopg named cursor (plain `DECLARE ... CURSOR`, so scrollable by default) over `WHERE ts > now() - '1 day'` holds the whole result in memory. PostgreSQL streams that query because its SeqScan can run backward.
   - Even an immutable SCROLL cursor keeps every later row after its first backward move. Moves that trigger this include `FETCH LAST`, `FETCH 0`, `RELATIVE 0` and a backward `ABSOLUTE`.
   - `FETCH LAST` also runs the query twice.
   - The memory test only covers `NO SCROLL`. Either add a bound test for a default cursor with a STABLE qual, or record this as an accepted limitation.

2. **MEDIUM (missing evidence) — integrated tree.**
   - The trial merge with 63f7c0b4 is textually clean. Conflict-free files on both sides: `catalog/system.rs`, `engine.rs`, `server.rs`, `session/cursor.rs`, `session/litplan.rs`, `session/mod.rs`, `paths/joinpath.rs`, `txn/mod.rs`.
   - **scan_need:** the shared `Exec::pull` → `open_need(scan_need)` path is kept. caab does not touch `pull`, and `rerun_to` clears the node so the restart reopens through `open_need`.
   - **e421:** its hooks don't overlap caab's hunks (`engine.rs` `repl.opened` after prepared recovery; `txn/mod.rs` `copy_batch` and `current_cid`).
   - **Cost:** caab has no chosen-cost or planner-cost change. The `joinpath` edit only changes the `EqScope` type, and `perms.rs` only defers the owner lookup.
   - **Not yet run:** the focused tests (twophase, the cursor goldens in all 3 modes, memory) on the merged tree.

3. **LOW — `cursor.rs:1300-1310` with `sql/expr.rs:803`.**
   - The restart safety check misses user or non-immutable functions that are not wrapped in `Expr::Call`.
   - Missed forms: operator `FuncDef`s inside `InList`, `ArrayOp` (including index `any` bounds), `IsDistinct`, `NullIf`, `MinMax` and `RowCompare`; domain CHECK expressions (`walk` does not descend into `DomainCoerce.checks`); and `Lazy::coerce` casts on PL/pgSQL cursors.
   - In any of these cases a restart could repeat notices or produce different values.
   - Needed: a PostgreSQL 18.6 golden with a NOTICE-raising STABLE `=` used in `IN (...)` and `= ANY`, plus a domain check, on a SCROLL cursor.

4. **LOW (PLAUSIBLE, unverified) — `cursor.rs:1335-1336`.**
   - The new default-scroll rule for `Join{left: One, cond: None, hash: None, nleft: 0}` ignores the join `kind`.
   - If the binder ever produces Semi or Anti in that shape, the cursor becomes scrollable where PostgreSQL's Result node (no outer plan) is not.
   - The `from_call` rule at `:1334` is correct: that Join is always `One ⋈ body`, i.e. EXPLAIN's Function Scan.

5. **LOW / nit — `cursor.rs:1149`.** `expect("streamed current key")` is a panic on a production path. The invariant holds as far as I can read, but it should return an internal error like `:1254` does.

## Evidence still needed before source-ready
- The full exact-source gates (`cargo build`/`test`/`clippy`, `pnpm test:pg`) passing on caab and on the caab ⊕ 63f7c0b4 tree, plus the focused tests re-run on the integrated tree.
- **Cursor goldens:**
  - The first backward fetch after the transaction's own `UPDATE`/`DELETE ... WHERE CURRENT OF`, so the restart has to use the DECLARE-time snapshot.
  - The multi-reference error on a `NO SCROLL ... FOR UPDATE` join after rows have been discarded.
  - A keyed `NO SCROLL WITH HOLD` cursor, to exercise the `persist` current-key path.
- **Lock test gaps:**
  - A waiter blocked on the prepared transaction's own lock that came from the fast path, woken by `COMMIT`/`ROLLBACK PREPARED`.
  - The originating session disconnecting between PREPARE and the finish.
- **OLTP165 numbers:** no benchmark has been run on this source yet.

The 149 scan serialization 6003 vs 6000 result is still unattributed. As asked, I didn't request a duplicate SSI test.
