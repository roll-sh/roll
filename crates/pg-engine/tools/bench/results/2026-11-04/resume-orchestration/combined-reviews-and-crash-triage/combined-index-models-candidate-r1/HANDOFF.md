# Untested combined index/models candidate

Status: WIP only. No compilation, tests, formatting, services, containers, benchmarks,
new Cargo target, or target directory were run or created. No readiness, source
acceptance, runtime proof, or performance claim. The parent schedules all execution.

Candidate: `4e588363d065aa9f12dc1b987d22dbf6742dcb9f`
Tree: `9013eae0ab2902d1ed66e6038ef0494574f1b331`
Branch: `feat/combined-index-validation`
Worktree: `/home/meszmate/worktrees/roll/t3-d62e3d3b`
Base: `fe7b0023c9df19a6dae1e6067ba8454998db48c5`
Dedicated backup: `origin/refs/heads/wip/combined-index-models-d62e3d3b`
The exact remote ref was verified with `git ls-remote`; no PR or shared-source push.

## Commit sequence

1. `c0805f3441d386c3686d358b275d5f77ed78af9a`: owner foundation, explicitly
   unvalidated with the modeled early-return HIGH still present. The explicit
   dispatch gate is already correct here.
2. `b4dfee71aafd1c291ebccaf5e44173ada3474029`: scoped observer and discriminating
   test definitions, unrun. This checkpoint still has the HIGH; no red test claim.
3. `4e588363d065aa9f12dc1b987d22dbf6742dcb9f`: shared size validation before the
   modeled return, global reduction and finalization, supplemental merge-order
   assertions. Unrun; no green test claim.

The parent may squash these before landing. Final diff: 38 files, 2694 insertions,
179 deletions, including all requested owner goldens. No old evidence/docs imported.

## Inputs and resolved overlaps

Actual merge bases were computed with `git merge-base` against fe7:

- index `483b4d6e5d41e9025aaa518d5774abd7971f4204`, base
  `d119ef44fedb3e020977e2dc09b721f481211b31`: 17 source paths, +692/-102.
- models `c66a076833291eaad99bdcfa7ed885ed2a68cb2c`, base
  `5c31223568fead896be10e66edd59e3d5a261002`: 9 source paths, +402/-58.

Applied source/test diffs as patches, never owner whole-file replacements. Textual
rejects resolved in exec/mod.rs, sql/costs.rs, parbuild.rs, and table.rs. The module
and planner context resolutions preserve newer shared code. The parbuild/table
resolutions preserve index163's serial callback, deferred duplicate and size checks,
concurrent preflight/copy names, and stores_entries policy, while retaining 170's
ParBuilt/model-key/counting plumbing. The two kept E1/E2 test improvements were
ported separately; 047's VACUUM source and test were excluded. No 167/165/166/169/148/149
owner range was applied.

The explicit gate is ((measures && btree) || (writes.is_some() && stores_entries))
&& !quad. Workers share one btree_size(types, expanded key+INCLUDE, FormatCtx), then
one BuildSizes per Gathered. All participants, including leader, are merged;
global unique_check precedes finish; finish precedes model/measured installation
and put_runs. Immediate 8191 formation and deferred minimum encoded full entry key
keep their ordering. Candidate size/data key stay together. GIN remains separate
and ordered with its existing byte/C/single-batch limitations. The obsolete
btree_oversized helper and worker threshold copies were removed. The modeled decode
remains partial; INCLUDE is excluded from uniqueness/model key-value arity.

## Prepared proof definitions, not executed evidence

The test is in the existing exec::sizing::kept::tests libtest module. Its observer
is compiled only with cfg(test), lives in exec/rowsize/observe.rs, and captures an
Arc from a caller-thread scope. There is no process-global cross-test counter.

The test requires requested/reserved/spawned=2, actual distinct leader and worker
threads with nonzero rows and validations, modeled-return counts on modeled
fixtures, expanded INCLUDE sizes, participant minima, merged minimum, and complete
query outcome. Fresh engines assert estimated/model state before every build.
It compares full Error values between 0/2 workers, including objects/detail/hint.

P0-P4 definitions cover INCLUDE 2816/8320, duplicate plus later 8191 formation,
duplicate winning over a real 2704 candidate, a duplicate-only twin, and ASC/DESC
minimum selection with distinct candidate owners and a winner after the first fold.
The direct merge-order assertions use the actual traced encoded candidates in both
orders. These definitions also run the measured fixture variants of P5.
The serial-only measured INCLUDE owner test is preserved exactly.

A five-second bounded coordinator gives leader/worker1/worker2 the first three
chunks in order and waits for all three first chunks to complete before allowing
more work. It aborts on spawn/reservation failure and participant errors. Proofs
fail if the three intended candidates are not owned by distinct participants.

Uncertainties: no compile check; no runtime validation of SQL fixtures, asserted
sizes, observer coordination or candidate placement. The new 2880-byte payload's
2896-byte expected tuple size is arithmetic, not a newly recorded PG oracle.
The first-chunk placement expectation relies on these fresh sequential row keys;
assertions reject missing coverage. Five seconds can expose scheduling/resource
contention as proof failure, which must not be waived as a pass.

## Preserved code and static checks

preservation.json records 27 exact before/after byte comparisons: full untouched
storage/sst.rs, repl/decode.rs, types/binary.rs, accepted a162 goldens; COPY loads,
Loading, put_fresh/note_load/start_load; read_once/in_read_once; accepted ANALYZE
heap_tuple_stats and DML failed-insert correction; all imported owner goldens and
the isolation spec. candidate-manifest.json records every changed committed blob,
SHA256, length, tree, branch and head, checked against the worktree bytes.

Only static source review, byte comparisons and git diff checks were performed.
Plain git diff --check fe7..HEAD reports the six imported golden .out terminal
blank lines. Their exact owner bytes are intentional. With only blank-at-eof
excluded, the same check passes. git diff --check for the validator change passes.
The worktree is clean after the final commit.

## Parent commands, UNRUN

Use the parent's existing target, resource wrapper and pinned reference service.
Do not create another target directory, service or container for these commands.

```sh
cargo test -p pg-engine --lib combined_index_build_validation -- --nocapture --test-threads=1
cargo test -p pg-engine --lib header_pages_are_available_before_layout -- --nocapture --test-threads=1
cargo test -p pg-engine --lib missing_tree_discards_header_before_row_rebuild -- --nocapture --test-threads=1
cargo test -p pg-engine --test models_kept -- --nocapture --test-threads=1
cargo test -p pg-engine --test big_tables measured_serial_include_size -- --nocapture --test-threads=1
cargo test -p pg-engine --test big_tables parallel_index_builds -- --nocapture --test-threads=1
GOLDEN_FILTER=a163_ cargo test -p pg-engine --test golden -- --nocapture
GOLDEN_FILTER=a170_btree_empty_leaf cargo test -p pg-engine --test golden -- --nocapture
PGISOLATION_ONLY=roll-bitmap-recheck cargo test -p pg-engine --test pg_isolation -- --ignored --nocapture
```

After focused source validation, the parent's terminal build/test/clippy, full
relevant golden/isolation and Postgres service gates still apply. Preserve raw
terminal logs, actual participant traces, and exact source identity. For a causal
regression run, the second checkpoint is the unvalidated pre-fix candidate; no
red/green outcome is claimed here.

## Held gaps

P6 expression/partial fallback and P7 non-btree worker guards need broader tests.
P8 layout/page/height/query/NULLS NOT DISTINCT parity, P9 transaction visibility,
and P10 pgstat/progress/cleanup remain queued. The existing parallel_index_builds
owner test has weaker worker evidence and cannot replace these. P11 preflight
owner goldens were ported but need combined reruns. P12 deferred2704/GIN/expression
preflight extensions remain queued. Background kept bail/shape-failure paths and
prior kept-review L-a/L-b/L-c limitations remain disclosed.

The separate raw heap sizing gap remains in c66+index483. No 047 VACUUM followup was
integrated: dropped-column undercount, COPY Load raw-size packing, PLAIN large rows,
historical layout/compression/pinned-pointer fidelity and VACUUM cost/footprint
still need separate owner repair and proof. GIN collation-aware and multi-batch
ordering are not added. All performance and footprint gates remain held.
