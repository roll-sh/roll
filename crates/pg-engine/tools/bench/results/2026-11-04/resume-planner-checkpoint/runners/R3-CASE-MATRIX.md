# Cursor decisions before the R3 correction

These are three independent decisions. Plan backward support plus DECLARE resolves
whether backward movement is permitted. Observation of a predicate requires per-row
backward re-evaluation only if the cursor permits backward movement. Restarting a
query prefix requires every executed operation to be unobservable.

| Cursor / predicate | Backward allowed | Extract qual for recheck | Restart discarded input |
| --- | --- | --- | --- |
| NO SCROLL, immutable builtin qual | no | no | no |
| NO SCROLL, STABLE/custom/domain qual | no | no | no |
| NO SCROLL, volatile qual | no | no (forward execution handles it) | no |
| default non-backward plan | no | no | no |
| default backward-capable or explicit SCROLL, pure immutable builtin qual | yes | no | only for supported plain immutable scan/filter/limit/append |
| default backward-capable or explicit SCROLL, observable STABLE/custom/domain/volatile qual | yes | yes for supported scan shape | no; retain scan inputs for directional evaluation |
| default scalar/non-SETOF FROM function (Left+One+from_call) | yes (Function Scan) | as applicable | no; retain/materialize function output |
| default set-returning FROM function (Inner+from_call) | yes (Function Scan) | as applicable | no; retain/materialize function output |
| sort/function/CTE outputs | plan-supported | qual only where native scan supports recheck | no |

NO SCROLL must not retain rejected rows. Scrollable observable quals require retaining
raw scan input under the current architecture: selective default-scroll memory is
O(scanned), not O(result). Quantify explicitly against a selective NO SCROLL case;
this widening is necessary for PG backward re-evaluation correctness but remains an
unaccepted limitation pending parent review. Broad spill/backward-native redesign is
outside this bounded correction.

Pass the resolved backward permission to Lazy::for_cursor (or resolve in its callers),
so qualifier extraction is conditioned independently from replayability. Replay's
conservative expression guard remains cursor-local. Support Inner/Left+One FromCall;
exclude Semi/Anti and unsupported generic joins. Verify scalar PLpgSQL FROM againstPG18.6.

Prior own-write golden proves snapshot values, not restart-path coverage. Add a state
assertion that rows were discarded before own UPDATE/DELETE and first backward fetch.
Keyed WITH HOLD golden covers persistence/heldcursor error, not the current-key fallback.
