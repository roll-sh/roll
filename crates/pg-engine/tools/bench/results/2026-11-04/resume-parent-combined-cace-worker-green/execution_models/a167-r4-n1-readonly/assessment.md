# Executor R4 N1: cache-only assessment

**Status: static N1 hypothesis supported; runtime unconfirmed.** All output here is cache-only. No source edit, target access, build, test, reference/service, crash/storage probe, or child task ran. Parent owns the a170 target and CPUs 4-7,22-25 for the combined proof.

The R4 report's statements that d9e had not run are stale. Exact d9e focused passes remain sealed at `7eff5c45`, and exact 1b6 focused passes at `a313c23a`. These passes do not cover the new nested InitPlan shape and do not clear N1. Original crash101 and other acceptance holds are unchanged.

## First wrong state and named trace

At the start of an unparameterized `right_rows` collect, d9e snapshots keys by traversing the inner plan. It passes only `!ctx.outer.is_empty()` (`nodes.rs:4153`). This describes whether *any* enclosing frame exists, not whether a frame was created by work inside this collect.

1. The cached `n` input contains a scalar subquery correlated on `n.rk`. The walker includes its root because its parameters read a local column, then visits its plan with `enclosing=true` (`nodes.rs:3935-3944`).
2. Actual execution calls `run_with_params`; after evaluating parameters it allocates `Rc::new(params)` and pushes that frame with `with_outer` (`mod.rs:1342-1350`, push/pop at `1221-1225`). This happens for each correlated call, even for equal values.
3. MIN/MAX optimization can synthesize another `Expr::Subquery` with `params=[]` (`paths.rs:6936-6940`). Its limit/index plan can still read the correlated frame in its index bounds. The optimizer's comment at `6735` and executor comment at `mod.rs:1293` describe this case explicitly.
4. That nested empty-parameter plan takes `enclosing_cache`, not `subq_cache`, because `reads_enclosing(plan,0)` sees the outer bound (`mod.rs:1309-1325`). The cache compares the entire frame stack using `Rc::ptr_eq`, and retains its prior frames, so each fresh pushed frame causes a miss.
5. The walker nevertheless rejects the nested plan at the unconditional `!s.params.is_empty()` check. Its counters never enter the before-map. `rescan_delta` now iterates only that map, so the nested counter delta from the first `n` collection cannot be replayed for later outer rows.

**Concrete hypothesis for fixture A, contingent on actual MIN/MAX and cached-inner plan shape:** six outer rows × nine tested `n` rows gives correlated root loops54; the engine executes the nested Limit/index plan nine times during the first collection. d9e replays the root to54 but leaves the nested plan at9. PG is expected to report54 for both. These counts are predictions, not measurements. The row-only count for the defined data should be6.

## Relevant cache/frame semantics

| Runtime branch, in actual order | Cache lifetime | Relevance to an inner replay |
|---|---|---|
| `params=[]`, nonempty outer stack, and `reads_cte(plan)` | Early uncached `collect`; no parameter frame is added by that call | A query reading a CTE inside an added correlated frame may run again. Include the nested query only after PG shape proof; do not import an external CTE producer through its CteScan. N4 at the collect boundary remains a separate engine/PG consistency limit. |
| Nonempty outer stack; not ANY/ALL; either empty params + `reads_enclosing`, or all parameters are `outer_only` | `enclosing_cache` keyed by subquery id plus the whole `Vec<Rc<Row>>` identity | Exclude if tied only to frames already present at the collect boundary. It can miss and rerun when work inside the collect pushes a fresh frame. Empty params do not imply statement caching. |
| Remaining `params=[]` | `subq_cache` by subquery id in this ExecCtx | Exclude even inside a fresh correlated frame. A blanket inclusion of all nested empty-parameter plans would regress this case. |
| Remaining parameterized subquery, including parameterized ANY/ALL | Direct `run_with_params`, fresh pushed frame | Include the rescanned plan; traverse its descendants with one additional frame since the collect boundary. Empty-parameter ANY/ALL still falls through to statement cache unless an earlier branch applies. |

`outer_only` permits `Expr::Outer` but rejects local `Col`, another Subquery, and volatile calls. `reads_enclosing` also checks scan index bounds; its other expression holes are N3 and are not addressed here.

Nested `collect` uses the same ExecCtx when already inside a statement (`mod.rs:1671`, `1737-1741`), so `subq_cache` is not made fresh merely by a correlated `with_outer` call. `child()` does create empty frame/cache state (`mod.rs:921-938`), but separate child-statement/Context lifetimes are outside this narrow proposal.

A parameterized nested loop has the same relevant transition: its left input and parameter expressions use the current frames, while its right collection runs under a new `Rc` frame (`nodes.rs:4116-4117`). The current generic `plan_inputs` walk passes the same enclosing flag to both children. Memoize misses also take this branch; hits do not collect. This explains the frame issue but does not fix or claim coverage of the pre-existing Memoize/barrier replay limits.

## Prepared bounded fixture

`a167_subplan_frames.sql` is an unexecuted four-case draft using the existing six/three/nine-row a167 tables and one index. It is not installed in the owner worktree, has not been parsed by PG or the engine, and has no fabricated `.out` file.

| Case | Distinguishing fact to establish later | Shape prerequisite / limit |
|---|---|---|
| A: indexed correlated MAX | Correlated root and its nested frame-bound MIN/MAX InitPlan must both repeat; predicted PG54/54 versus d9e54/9 | Confirm cached inner `n`, correlated SubPlan, and nested InitPlan -> Limit -> index path. `enable_seqscan=off` encourages this but is not proof. If optimization chooses another shape, record non-discrimination rather than call it a red. |
| B: nested independent SUM | The correlated root repeats, but its truly statement-cached nested SUM stays once | Uses MAX of a local expression to avoid the named indexed MIN/MAX rewrite. Confirms a fresh frame alone does not make all nested subqueries replayable. |
| C: dependent local CTE plus nested MAX | A fresh correlated frame owns a parameter-dependent CTE; its nested parameterless CTE-reading query must be assessed separately from the producer | Confirm the outer SubPlan carries `n.rk`, the CTE depends on it, and the nested aggregate remains an InitPlan. This deliberately avoids N2's unparameterized local-WITH rewind case. No N2 closure is claimed. |
| D: pre-existing stable frame | An InitPlan tied to the already-pushed enclosing count-query frame stays excluded from that count query's inner replay | Expect once per enclosing SubPlan invocation, not once per reused `n` scan. Confirm the tiny `r` outer / cached `n` inner shape. Index access is disabled for this guard only. |

Keep the sealed R3 statement InitPlan, external CTE producer, and correlated54 controls as regression constraints. Do not overwrite their recorded expectations or original failed `.actual` evidence.

## Smallest proposed correction boundary, not implemented

Replace the single traversal notion of `enclosing` with both the base frame depth at the collect boundary and the number of parameter frames pushed on the path since that boundary. This is walker metadata only; do not change query execution, cache contents, ExecCtx constructors, or catalog APIs.

- Classify a subquery using runtime branch precedence: statement cache versus enclosing-frame cache versus direct/CTE read. In particular, distinguish an empty-parameter MIN/MAX plan with outer bounds from an independent empty-parameter InitPlan.
- Direct parameterized SubPlans add one frame while walking their body. An included enclosing-cache call adds a frame only if its own params are nonempty; empty-parameter MIN/MAX and CTE-read calls do not push one themselves.
- For a parameterized Join, walk left/parameter expressions at current depth and right at current depth +1. Ordinary unparameterized inputs do not add a frame. Keep producer ownership distinct from query cache lifetime.
- Frame-cached work tied to frames already present at collect entry stays excluded. Frame-cached work reached under fresh frames inside the input is an N1 inclusion candidate; true statement-cached work stays excluded at every depth.
- Counter keys remain deduplicated by node address. If a shared Arc can be reached with different frame context, do not use the stats map's address-only membership as the sole traversal visited set; otherwise a visit in the stable context can suppress later discovery in a fresh context. A separate visited traversal state or safe revisiting/union of keys is needed.

A small pure cache-policy helper shared with `subquery_rows_here` could prevent branch-precedence drift; that would be a scoped interface extraction, with no runtime caching behavior change. No helper or source patch has been written here.

**Limit on this design:** the engine invalidates enclosing cache by identity of the *whole* stack, even when a subquery only reads a frame outside the newly added portion. PG may track changed parameter dependencies more narrowly. Therefore `added_frames>0` is necessary to fix the named omission but is not a universal PG replay proof. The named fixture must establish actual relevant changing parameters and recorded PG loops before choosing a production condition.

## Handoff / next authorized boundary

Return this cache-only hypothesis/fixture to the parent while its combined proof owns the target and cores. No owner execution is queued. A future authorized probe must pin exact source and executable provenance, record actual PG18.6 shape/output and engine output/exits separately, preserve both red and non-discriminating outcomes, and only then justify the smallest correction. No broad suite, new optimization round, N2/N3 expansion, models parbuild edit, or crash/storage rerun is implied.
