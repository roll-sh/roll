# Untested scoped successor and corrected red reference

All source and test definitions below are UNRUN. No compilation, tests, cargo,
rustfmt, containers, services, benchmarks, new targets or new worktrees were used.
No source/performance acceptance. The source review judges the original HIGH fixed
statically at 4e, but the runtime obligation remains OPEN. The parent alone schedules
execution after the currently owned jobs release their targets and slots.

Final candidate:
- Commit e25c258e759feb28ccb247fa8b10cc25bc7e5810
- Tree be302075b45e497cd0148ca8152b210852164499
- Local branch feat/combined-index-validation
- New remote ref refs/heads/wip/combined-index-models-source-r1-d62e3d3b

Corrected TEST-ONLY red reference:
- Commit b43e47a5ba581ac72254d15f39bf7ae4389f35b3
- Tree f04a341addee3ae961c033347fdb57274006b2b7
- Parent b4dfee71aafd1c291ebccaf5e44173ada3474029
- Local/remote ref refs/heads/wip/combined-index-models-red-proof-r1-d62e3d3b
- Never run. The old modeled early return remains. Its predicted red is not evidence.

Proof-only candidate checkpoint:
- Commit ca45637beccaf8d4183ae1eb2bafc7227b9dd355
- Tree ebedc993b46071bd79548edfcddbb700f488270c
- Remote ref refs/heads/wip/combined-index-models-proof-r1-d62e3d3b

Original 4e and refs/heads/wip/combined-index-models-d62e3d3b are preserved at
4e588363d065aa9f12dc1b987d22dbf6742dcb9f. All four remote SHAs were verified with
git ls-remote. No PR, shared-source push, owner ref/index/source change, or parent
checkout edit. The only current checkout is the original isolated worktree.

## Per-commit scope

1. ca45637beccaf8d4183ae1eb2bafc7227b9dd355, parent 4e:
   three test/observer paths, +46/-18. max_parallel_workers is now 1024; the request
   still comes from max_parallel_maintenance_workers=2. Exact requested/reserved/
   spawned=2 and actual worker validations remain. Full trace prints before error
   extraction and appears in the unexpected-success panic. ASC must select worker2
   in the last fold; DESC must select worker1 in the middle. Every candidate has a
   distinct data/full encoded key, matches its participant minimum and expected own
   size, and the final selected candidate must equal the complete encoded minimum.
   Coordinator-aborted waiters get an explicit marker and partial Ok, not another
   participant's cloned row error. Reservation/spawn/timeout gate failures remain
   errors, checked before abort markers; waits remain individually bounded at 5 s.
   Current late-error cases reject any coordinator abort. No worker assertion waived.
2. 1b0f188115435dfa065148badd1a93aeb2ee056f, parent ca:
   exec/dml.rs and cfg(test) kept::tests, +66/-1. The two INSERT-only size-error exits
   now count the failed physical heap INSERT once. Stats regression definitions
   cover 2704/8191, multi-row statement rollback, one successful INSERT, ordinary
   UPDATE and ON CONFLICT DO UPDATE controls. New stats expectations are derived
   from the accepted accounting rule, UNRUN and UNORACLED. No golden fabricated.
3. e25c258e759feb28ccb247fa8b10cc25bc7e5810, parent 1b:
   exec/rowsize.rs only, +6/-0. Restores the 483 metadata/value guard ahead of types
   Vec construction. Non-ASCII and bpchar key/INCLUDE exclusions stay conservative;
   the worker-safe core remains shared. The original k.ty() metadata access and
   existing all-values vector remain; no new allocation benchmark or claim.

Final scoped delta from immutable 4e: 5 files, +118/-19. Overall fe7..final:
38 files, +2793/-179. Source and stats changes are absent from the corrected b4 red.
The red applies the exact same proof correction patch to b4 (+46/-18 in 3 paths).
It intentionally lacks 4e's later btree_row/merge supplement because those APIs did
not exist at b4. No production repair was backported into the red reference.

## INSERT accounting trace

index_entries_checked has exactly two callers, both in insert_prepared:
ordinary insertion and the speculative insertion attempt. Neither calls
count_insert for this attempt before index_entries_checked. A size error returns
immediately and cannot reach store_inserted's success count or the speculative
conflict success-result branch's count. The formation branch first marks dead with
inserted_with, then counts once. The third-page branch's TID closure already places
the dead tuple; its error branch counts once without calling inserted_with twice.
The accepted a162 unique-error bytes and count remain untouched.

update_row_rls has its own on_form/on_insert checks and update accounting; it does
not call index_entries_checked. ON CONFLICT's pre-check can select its UPDATE action
without a speculative INSERT attempt. Neither UPDATE path gets an inserted count
from this patch. A cross-partition move goes through delete_row plus insert_routed,
so its destination performs a physical INSERT just as under the accepted existing
success/unique-error semantics. No general UPDATE or conflict-flag redesign.
count_insert itself still honors the existing track_counts setting.

## Static checks and artifacts

candidate-manifest.json records final committed blobs, SHA256, length, tree and
per-commit paths/diffstats. All 38 changed final files match worktree bytes.
preservation.json retains all 27 before/after byte comparisons, including accepted
a162 hunks, COPY/read-once hooks and imported golden bytes. git diff --check on the
successor corrections passes. Plain fe7..final still reports the original six .out
terminal blank lines; excluding only blank-at-eof passes. Golden bytes unchanged.

../proof-r1-successor/production-preservation.json verifies exact production-byte
preservation for ca vs 4e and corrected red vs b4. Only the explicitly cfg(test)
observer file, existing kept::tests tail and cfg(test) first-chunk block are excluded;
all remaining bytes are equal. It also hashes red Plan::row as an exact b4 match.
The red was assembled with a private temporary GIT_INDEX_FILE and commit-tree,
without checking out b4 or changing this worktree's real index/branch.
Historical manifest verification reads committed objects. No runtime Rust parser,
compiler or test was used. Temporary Python static-audit printing initially hit a
path-variable shadow after saving its reports; that script was corrected and rerun
successfully. This did not alter source or Git identity.

## Uncertainties and held gaps

Nothing here proves compilation or runtime behavior, including the coordinator and
new stats fixtures. Raising the session allowance avoids the two-slot collision;
it does not isolate the process-global ACTIVE counter or waive failures under
extreme contention. Every wait has its own 5 s deadline, not one total build budget.
Candidate ownership still depends on these fresh rowids/chunks and fails closed.

2896, 3016 and selected DETAIL TIDs remain arithmetic plus serial/worker self-parity,
not captured PG18.6 expectations. New failure stats likewise need PG capture.
FORMAT-AUDIT.md records the bounded read-only audit: FormatCtx is explicit for
format_value/array/range output, but relsize non-ASCII byte sizing reads the database
encoding through thread-local DEFAULT, which fresh build workers do not initialize.
This is a concrete source dependency and unresolved non-UTF8 correctness risk;
there is no new formatting/locale fix or executed counterexample here.

The separate raw Load/heap problem is retained. The user reports confirmed owner170
reds: dropped-column live pages 33 versus PG 667; COPY with five 2000-byte fields or
MAIN 8150 overflows. No 047 VACUUM followup or later owner170 repair is included.
The modeled 8300 INSERT SELECT fixture may fail at raw Load before CREATE INDEX;
that is a missing proof, never a skip/pass. The parent may separately apply the
focused Load repair if needed, with its own source identity and evidence.

Other disclosed LOWs remain: serial batch writes continue after a known deferred
failure; simultaneous immediate participant-error order is not heap order;
pre-existing measured count_returned gap remains. P6/P7 fallback/method guards,
P8 layout/pages/height/NULLS parity, P9 visibility, P10 progress/stats/cleanup, P11
combined reruns and P12 extra preflight cases remain queued. GIN byte/C and single
batch ordering, kept-review residuals, historical toast layout fidelity, PLAIN
large rows and all performance/footprint gates remain held.

## Parent execution proposals, never run here

Use the parent's existing resource wrapper/targets and reference service only.
First run the focused proof on corrected b4 red, expecting a predicted red with
retained trace; then on ca, or final with its separately disclosed production delta.
A clean red/green result cannot be claimed until both raw terminal runs exist.

```sh
cargo test -p pg-engine --lib combined_index_build_validation -- --nocapture --test-threads=1
cargo test -p pg-engine --lib combined_index_size_error_stats_unoracled -- --nocapture --test-threads=1
cargo test -p pg-engine --lib -- --nocapture
cargo test -p pg-engine --test big_tables measured_serial_include_size -- --nocapture --test-threads=1
cargo test -p pg-engine --test big_tables parallel_index_builds -- --nocapture --test-threads=1
cargo test -p pg-engine --test models_kept -- --nocapture --test-threads=1
GOLDEN_FILTER=a162_stats_dead cargo test -p pg-engine --test golden -- --nocapture
GOLDEN_FILTER=a163_ cargo test -p pg-engine --test golden -- --nocapture
GOLDEN_FILTER=a170_btree_empty_leaf cargo test -p pg-engine --test golden -- --nocapture
PGISOLATION_ONLY=roll-bitmap-recheck cargo test -p pg-engine --test pg_isolation -- --ignored --nocapture
```

Capture PG18.6 expectations separately for the corrected minimum ASC/DESC and size
failure stats fixtures before parity claims. Add focused non-UTF8 coverage for the
encoding dependency. Only after the parent's resource and narrow-proof gates allow
it, the terminal build/test/clippy and pnpm test:pg gates still apply; none is
waived or authorized to run by this handoff.
