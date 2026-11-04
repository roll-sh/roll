# Combined index163 / models170 validator design — preparation only

Recommendation: carry 170's modeled-build outputs through a single per-participant
`BuildSizes`, using the same btree size calculation and deferred-error reduction as
163's serial build and concurrent preflight. Validate before the modeled early return.
Do not copy either owner's whole files onto the shared base.

This is a static design, not an applied patch, test result, acceptance, or performance
claim. No source, ref, index, HEAD, existing evidence, process, or affinity was changed.
No build, test, container, benchmark, target, worktree, or owner polling was started.

## Inputs and source map

Read first: cache `index-source-review-r3-result.json`,
`models-source-review-r2-result.json`, `models-source-review-r3-result.json`.

Pinned objects:

- Shared: `fe7b0023c9df19a6dae1e6067ba8454998db48c5`.
- Index: `483b4d6e5d41e9025aaa518d5774abd7971f4204`, relative to
  `d119ef44fedb3e020977e2dc09b721f481211b31`.
- Models: `c66a076833291eaad99bdcfa7ed885ed2a68cb2c`.
- Models followup: `047c4697e72ce63867257db3668a02c55c77830a`;
  this only changes VACUUM, kept tests, and its new VACUUM test relative to c66.

All paths below are under `crates/pg-engine/`. Line numbers name committed objects,
not the live working tree; use the named function as the patch anchor.

| Area | Exact anchors | Responsibility |
|---|---|---|
| `src/exec/rowsize.rs` at 483 | `btree_size` 53; `on_form` 72; `on_insert` 86; `btree_oversized` 99; `BuildSizes` 179, `row` 185, `finish` 206 | Expanded key + INCLUDE sizing; immediate btree formation failure; deferred minimum btree page failure; separate ordered GIN error |
| `src/session/ddl/parbuild.rs` at c66 | `Gathered` 78; `Plan` 88; `Plan::row` 107, modeled branch 110–119; `Plan::run` 153; `ParBuilt` 243; `build_index_parallel` 261; reduction 357; `unique_check` 398; `put_runs` 388 | Worker-safe data, chunk scans, global uniqueness, installation |
| Same file at 483 | `Gathered::oversized` 84; checks 122–128; reduction 338–355 | These are 163's checks that must be absorbed into the shared accumulator, not left only below 170's return |
| `src/session/ddl/table.rs` at c66 | `build_index` dispatch 3764–3787; serial scan 3794–3809; `counted` 3830 | Modeled/measured result plumbing and fallback |
| Same file at 483 | `index_build_error` 3607–3665; `build_index` step 3732; size/unique finalization 3833–3837; concurrent REINDEX 9535 | Preserve row callback, deferred duplicate, preflight, copy naming, INVALID state |
| `src/session/ddl/vacuum.rs` at 483 | `measures_in_build` 1394; `measure_rows_with` 1406; `each` callback 1583–1584 | Callback receives expanded row before heap packing; measurement remains available for non-btree built-ins |
| `src/exec/scan.rs` at 483 | `stores_entries` 266; `decode_table_row_need` 95 | Entry storage policy; required INCLUDE columns decoded |
| `src/exec/sizing.rs` at c66 | `index_key_of` 1336; `btree_from_build` 1391; `index_columns` 2146 | Existing model key formation, write-generation/TID guard, key + INCLUDE decode mask |

## Flow and invariants

`Ddl::build_index` binds the index, starts stats/progress, then chooses parallel,
measured callback, or ordinary serial scan. Serial and measured callback use the same
`step(dkey, key_values, expanded_row)` from 163. Concurrent preflight separately scans
through `index_build_error` with the same `BuildSizes` contract.

Parallel participants receive only owned/thread-safe projection metadata and
`FormatCtx`. They must not receive `ExecCtx`, `Ddl`, session `RefCell`s, or a new
`BoundIndex` reference just to validate size. Keep `Plan`'s plain-column/no-predicate
restriction. `index_columns` already includes INCLUDE; retain its mask.

Required btree ordering:

1. Decode/evaluate and form each qualifying tuple. A size >8191 returns 54000
   immediately from that row. Duplicates and >2704 candidates do not stop the scan.
2. After all participant results are collected successfully, run the existing
   `unique_check` across all sorted participant entries.
3. Then report the minimum encoded full entry key with size >2704, retaining that
   candidate's own size and data key for the leader's `row_tid` lookup.
4. Only then call `put_runs`, report loaded tuples, or return `ParBuilt::Modelled` /
   `ParBuilt::Measured` for model/stat installation.

The minimum is by `index_entry_key` including the row tail, not heap visit order,
participant completion order, measured `key_bytes`, or size. Preserve ASC/DESC and
NULL ordering through that encoder. A participant can retain only its local minimum:
`min(min(P0), min(P1), ...)` equals the global minimum. A deferred error must never set
the shared stop flag or call `finish` in `Plan::run`.

GIN remains a separate accumulator ordered by `(column, extracted key)` using the
existing `GinFormError::precedes`. Both GIN limits are deferred in builds under 163's
current contract; do not impose the btree immediate-8191 rule on GIN. Keep the accepted
byte/C-order and single-batch limitations explicit. No parallel GIN path is introduced.

## Type/signature sketch

These are interfaces and pseudocode, not an assembled implementation.

Two shapes considered: a serial preflight before every parallel build would reuse
163 unchanged but add a second scan/evaluation and leave the worker path intrinsically
unchecked; it is rejected. The chosen shape moves only the session-free sizing core
and accumulator operations into `rowsize`, retaining a single worker scan and the
existing leader's error formatting. Copying checks into both worker branches would
be smaller locally but retain the exact drift that caused this HIGH.

```rust
// rowsize.rs: extract the type/value/format-only core of existing btree_size.
// all/types contain key attributes followed by INCLUDE attributes in the same order.
// None means the conservative small-value bound proves size <2704.
pub(crate) fn btree_size(
    types: &[DataType], all: &[Value], fmt: &FormatCtx,
) -> Option<usize>;

// Private adapter for on_form/on_insert and BuildSizes::row: assemble types/all
// from t/bix/key vals/expanded row, then delegate to btree_size. No storage-pointer size.
fn btree_size_of(
    t: &Table, bix: &BoundIndex, vals: &[Value], row: &[Value], ctx: &ExecCtx,
) -> Option<usize>;

// Existing fields remain: btree minimum (key, bytes, dkey), separate private GIN error.
impl BuildSizes {
    // Shared classification/reduction. Called by row() and by parallel participants.
    // Some(n)>8191 => immediate form_error(n).
    // Some(n)>2704 => lazily form entry_key(), retain minimum with n and dkey.
    pub(crate) fn btree_row(
        &mut self, size: Option<usize>, entry_key: impl FnOnce() -> Vec<u8>,
        dkey: &[u8],
    ) -> Result<()>;

    // Btree uses the same encoded-key minimum; GIN uses its own precedes comparator.
    // This method never formats an error, consults a session, or finishes validation.
    pub(crate) fn merge(&mut self, other: Self);

    // Existing signatures retained for serial, measured callback, and preflight.
    pub fn row(&mut self, t: &Table, bix: &BoundIndex, vals: &[Value],
        row: &[Value], dkey: &[u8], ctx: &ExecCtx) -> Result<()>;
    pub fn finish(self, t: &Table, bix: &BoundIndex, ctx: &ExecCtx) -> Result<()>;
}

// parbuild.rs
#[derive(Default)]
struct Gathered {
    // Retain entries, keys, parts, and 170's model keys.
    sizes: BuildSizes, // replaces 163's separate oversized field/reducer
    // ...
}
```

`btree_size` retains `surely_small`, the non-ASCII fallback, and the bpchar exception
checked against the entire type list (key and INCLUDE). Otherwise it calls the same
`relsize::index_tuple` with the same format context. DML's `on_form` / `on_insert`
delegate via the private adapter; retain their before/after-uniqueness behavior.

`BuildSizes::row` matches method once: btree calls `btree_size_of` once and feeds
`btree_row`; GIN keeps its existing ordered accumulator; other methods keep `on_form`.
This removes 483's duplicate btree formation (`on_form` then `btree_oversized`). Once
its only build caller is migrated, remove unused `btree_oversized`, not a second API.

Caller-first worker sketch:

```text
decode row using existing modeled/measured choice (expanded values)
vals := key values only
k := index_entry_key(key vals, dkey, colls)
checked := current NULLS NOT DISTINCT rule, inspecting key vals only
all := key vals followed by row[include] values
size := rowsize::btree_size(ix_types, all, fmt)
g.sizes.btree_row(size, || k.clone(), dkey)?
g.entries.push(k, tail, checked)
if model:
    g.model.push(index_key_of(key vals only, expanded row), dkey)
    return Ok(())
else:
    exact := size.unwrap_or_else(|| relsize::index_tuple(ix_types, all, text))
    g.keys.push(key-only sort prefix/bytes, exact)
    retain existing heap-size part collection
```

The measured fallback computes an exact size only for a tuple already proved below
all error limits. Wide tuples use the shared exact result; no separate threshold
logic remains in parbuild. Do not pass `all` into uniqueness, the sort key, or
`index_key_of`: INCLUDE is not a key. Do not use model `Key.size`, heap TOAST pointers,
or `stored_tuple_len` as the index's validation size.
Keep modeled decoding partial (`model || stored.is_some()` selects the needed-column
decode); do not make `stored=None` accidentally force a full modeled-row decode.
The serial measured callback still sizes again for its layout bookkeeping; returning
sizes through that callback is a separate optimization, not needed for this fix.

After worker joins, fold `sizes.merge(g.sizes)` for every actual `Gathered`, including
the leader, then `unique_check`, then `sizes.finish(t, bix, ctx)?`, before the modeled
return and measured page calculation/install. Resolve any real worker errors first
using existing `Result`/stop behavior. Multi-error scheduling determinism is not added.
The outer serial accumulator is empty when parallel ran; its existing `finish` is
harmless and does not substitute for this worker reduction.

Dispatch in `table.rs` must be exactly equivalent to:

```rust
let measures = self.measures_in_build(t, ix);
let writes = (ix.method == "btree")
    .then(|| sizing::heap_writes(t.oid, &ctx.sess.db)).flatten();
let eligible = ((measures && ix.method == "btree")
    || (writes.is_some() && scan::stores_entries(&bix))) && !quad;
```

Keep 170's `model = !measures` argument, `ParBuilt`, `par_counted`, `model_keys`,
`counted = par_counted.or_else(...)`, and no-second-scan fallback. Keep 163's
`stores_entries(&bix)` in serial batch insertion; do not restore `ix.id != 0`.
`measures_in_build` still handles other built-in methods through the serial callback.

## Hunk order onto fe7 and conflict handling

1. **Index foundation, selective diff from d119..483:** add `exec/rowsize.rs` and its
   module declaration; retain 163's relevant DML formation/insert hooks and
   `scan::stores_entries` policy/callers. Bring the row-aware measured callback and
   serial `BuildSizes`/deferred-duplicate logic. Change both callback invocations
   (`measure_rows_with` and `build_index` serial scan). Preserve unrelated shared code.
2. **Concurrent fix, same index range:** rename `index_duplicate` to
   `index_build_error`; include `bix.index = Arc::new(ix.clone())`, size scan and final
   ordering; update CIC in `objects.rs` and INVALID concurrent REINDEX in `table.rs`.
   Preserve the commits of `valid=false, ready=false`, unique copy naming via
   `choose_name(object_name(..., "ccnew"))`, and retry suffix behavior.
3. **Model foundation, owner-scoped hunks:** retain c66's sizing/kept repair and
   170's `ParBuilt`, model key collection, `index_built_from` result plumbing and
   `btree_from_build` write-generation guard. Do not select the non-btree worker gate
   or pre-163 `step` body from the models file.
4. **Resolve combined overlap as one coherent hunk group:** extract the worker-safe
   sizing core, add `btree_row`/`merge`, replace `Gathered::oversized` with `sizes`,
   move shared row validation before `if model`, reduce before both output branches,
   and use the explicit dispatch above. No transient combined source should be
   presented as validated between steps 3 and 4.
5. **Port owner tests and add the focused proofs in proof-matrix.tsv.** Record exact
   combined source identity and proof-hook changes separately. Only the parent runs
   checks after source stabilizes under the existing resource plan.
6. **Separate VACUUM followup:** evaluate the repaired successor to 047 independently;
   it is not part of the combined-validator fix or a precondition for fixtures that
   never VACUUM wide rows. Retain kept-test E1/E2 improvements independently.

Likely textual/semantic conflicts:

- `table.rs::build_index`: `step` arity/body; eligibility; `ctx`/`writes` placement;
  `model_keys` lifecycle; `par_counted`; final duplicate/size ordering.
- `parbuild.rs`: `Gathered` fields; `Plan::model`; early return; return type/argument;
  local/global oversized state; `put_runs` extraction. Keep one reducer and finalizer.
- `vacuum.rs`: 163's callback and `kept_live` hunks, 170's `index_built_from` plumbing,
  and 047's later heap-layout changes. Preserve the shared ANALYZE dead-row correction
  (`heap_tuple_stats`, fe7 lines 551–562). Similar nearby text is not replaceable wholesale.
- `sizing.rs`/`kept.rs`: preserve c66's header invalidation and restored-shape guards
  alongside shared code. No legacy owner whole-file restore.
- Shared COPY / `read_once`: fe7's `txn/mod.rs` load hooks (`loads` 1030, `Loading`
  1039, `put_fresh` load accounting 1888–1892, `note_load` 1973, `start_load` 2023)
  and shared read-once hooks must survive owner diffs: `storage/mod.rs::read_once`
  213 / `in_read_once` 224, `storage/sst.rs` cache-fill gate 1082, and
  `repl/decode.rs` wrapper 80, all at fe7. No change to these is needed for the
  validator. Pin their hunks/blobs separately in parent integration review; owner
  file snapshots predate them.

## Proof contract and remaining risks

See `proof-matrix.tsv` for exact discriminating cases and existing coverage. New
focused tests can live in an existing target; no new Cargo target is necessary.

The authoritative real-worker proof must record `requested=2`, reserved `got>0`
(preferably exactly 2), successful thread starts, and nonzero rows processed on actual
worker threads. `index_builds()` increments immediately after reservation, even if
`got=0`, and `.spawn_scoped(...).ok()` can drop failed spawns: its delta is insufficient.
Settings alone and the unrelated parallel-scan `started()` counter are insufficient.

For deterministic cross-participant coverage, use a bounded, `cfg(test)` observer in
the existing library unit-test target: a caller-thread scoped trace captured into
`Plan` via an `Arc`, not process-wide mutable counters. Record build identity, model
flag, requested/reserved/spawned counts, each participant/chunk, checked/validated
rows, its local page-error minimum, the merged minimum, and outcome. Trace failure
paths as they happen so an 8191 error is observable despite losing `Gathered` to Err.
A test-only first-chunk gate can require the leader and two successfully spawned
workers to claim distinct chunks before continuing; abort it on spawn failure and
bound the wait. Without that gate, assert distinct candidate-owning participants
from the trace and treat a same-participant run as missing coverage, never a pass.

For minimum-key reduction, ensure at least two participants retain different >2704
candidates; the global winner must not be the first candidate folded. Test both
merge orders directly as a small supplement, and capture a real-worker run whose
trace proves this condition. Distinguish candidate placement from mere row counts.

Keep exact error code/message/detail/hint/table/constraint, including the selected
row's `(block, offset)`, in serial-vs-worker comparisons on identically prepared heaps.
Use fresh fixtures per mode so a previous build/VACUUM does not seed a model or hide
the selected path. Assert `is_estimated` plus presence/absence of `heap_writes` before
each build and trace the measured callback where required.

No extra requirement is added for deterministic ordering among multiple simultaneous
decode/formation/cancel errors. Duplicate selection remains the current
`unique_check` policy (earliest second data-key occurrence), not a redesign to sorted
duplicate selection. Page/height/layout parity, pgstat, NULLS NOT DISTINCT, progress,
and transaction visibility remain required broader 170 integration evidence; they
must not be claimed from the size-only proofs or old estimated-table tests.

047 is not ready to treat as the complete TOAST repair: R3 still identifies dropped
column undercount in its fallback; raw-size packing remains in COPY `Load::row`;
PLAIN >page insertion, historical storage/compression/pinned-pointer fidelity, and
VACUUM performance/footprint remain held. Repairing that requires a separate layout
contract and proofs. Do not silence HeapPacker by clamping or skip index validation;
do not pull shared COPY changes into this validator patch as an opportunistic fix.
Keep a VACUUM followup confined to `vacuum.rs`'s heap decode/packing region (047
1599–1646 and its decode guard) plus `vacuum_toast_measure` cases. It must not change
the index `each` callback contract or its position before heap packing. For the
reported dropped-column regression, preserve existing-row `heap_tuple` sizing with
only virtual columns nulled at <=2032 bytes, and use layout above that threshold;
historical toasted/dropped rows still need an explicit accepted approximation or a
stronger stored-layout model. COPY `Load` is another separately owned followup.

## Parent execution recipe (not run)

Use the parent's existing build target/resource wrapper and pinned reference service;
these commands are the inner test invocations, not authority to create a new target,
container, or service. Proposed focused library test name:
`combined_index_build_validation`; put it under an existing `#[cfg(test)]` module so
the scoped observer has no production API. Its parameter cases implement P0–P10.

```sh
cargo test -p pg-engine --lib combined_index_build_validation -- --nocapture --test-threads=1
cargo test -p pg-engine --test big_tables measured_serial_include_size -- --nocapture --test-threads=1
cargo test -p pg-engine --test big_tables parallel_index_builds -- --nocapture --test-threads=1
GOLDEN_FILTER=a163_review_sizes cargo test -p pg-engine --test golden -- --nocapture
GOLDEN_FILTER=a163_review_concurrent cargo test -p pg-engine --test golden -- --nocapture
GOLDEN_FILTER=a163_index_row_size cargo test -p pg-engine --test golden -- --nocapture
```

For new PG-reference expectations use the same existing PG18.6 reference harness
with an ordered payload helper. Do not overwrite the old goldens during proof.
Equivalent core fixture SQL (each length/mode gets a fresh fixture):

```sql
CREATE FUNCTION ci_text(n int) RETURNS text LANGUAGE sql IMMUTABLE AS $$
  SELECT left(string_agg(md5(g::text), '' ORDER BY g), n)
  FROM generate_series(1, (n+31)/32) g
$$;
CREATE TABLE ci(k int, payload text);
INSERT INTO ci SELECT g, CASE WHEN g=6000 THEN ci_text(2800) ELSE 'small' END
FROM generate_series(1,6000) g;
CREATE INDEX ci_i ON ci(k) INCLUDE(payload);
-- Expected: 54000, index row size 2816 exceeds ... maximum 2704.
-- Fresh fixture with ci_text(8300): 54000, requires 8320 ... maximum 8191.
```

Retain raw logs with combined commit/source manifest, trace/proof-hook identity,
per-case path/state assertions, actual worker counts/rows/minima, complete error
fields, and terminal exit status. A green size assertion without its required path
trace is incomplete evidence. Parent's full terminal gates remain separate.

Parent completion gates remain combined build/test/clippy, relevant golden/isolation
and Postgres service checks, and separately owned performance/footprint/parity gates.
Nothing in this artifact closes them.
