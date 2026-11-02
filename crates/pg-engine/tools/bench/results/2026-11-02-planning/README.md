# Prepared-range planning diagnostic

Forced generic mode exposes a large mode-sensitive cost in the checked ordered-range
workload. This diagnostic changes no engine code or production settings. It does not
isolate planning CPU or recommend forcing generic plans in production.

The same frozen histogram engine and checked client run on independent clones of a
stopped 200,000-account fixture. Each block has one warmup pair followed by five
alternating ten-second pairs. Server CPUs are 28–31, and client CPUs are 32–35.
Every timed operation checks every returned value. Fixture digests match before and
after every block. All 84 invocations complete with zero errors and 4,802,187 checked
operations, including warmup. This total combines accounting across blocks only.

## Clean mode controls

Values below are medians of five measured samples, with minimum and maximum
throughput in parentheses. CPU includes each complete client invocation and setup.

| Workload | Auto operations/s | Generic operations/s | Auto / generic server CPU, µs/op |
| --- | --- | --- | --- |
| Range, 1 client | 1,991 (1,967–2,003) | 2,762 (2,702–2,804) | 415.39 / 277.39 |
| Range, 8 clients | 8,810 (8,787–8,883) | 12,765 (12,705–12,888) | 448.28 / 303.14 |
| Point, 1 client | 6,846 (6,702–6,923) | 6,773 (6,695–6,920) | 111.44 / 111.32 |

Every eight-client range pair favors generic, with throughput increases of 43.0–46.0%
and CPU per operation reductions of 31.5–32.7%. Approximate aggregate server use is
3.94 cores for auto and 3.87 for generic, consistent with a four-core server CPU
limit. Client use is about 0.84 and 1.19 cores out of four, respectively.

Auto and forced-custom range controls overlap. At eight clients they reach
8,892 (8,852–8,927) and 8,853 (8,813–8,889) operations/s, with 444.35 and
445.31 µs of server CPU per operation. At one client they reach 2,015 and 2,006
operations/s, with 409.93 and 411.22 µs of server CPU per operation.
Forced-custom point CPU rises from 110.96 to 112.77 µs/op in this control.
Its custom-plan counter does not prove fresh binding, because the point path can
reuse a generic binding while counting a custom plan.

Process RSS has no consistent benefit across the blocks. For the eight-client
auto/generic range comparison, medians are 111.3 and 112.2 MB. The captured summaries
retain every RSS range. No memory-efficiency claim is established.

Preflight uses the exact point/range SQL through a named extended-protocol statement,
INT4 parameters, binary results, and twenty value-checked executions. Auto range
counts end at 0 generic / 20 custom. Auto point ends at 15 / 5. Forced modes end
at 20 / 0 or 0 / 20. Separate EXPLAIN probes preserve complete verbose JSON plans.
Range plans differ only in `Index Cond`, `Plan Rows`, and `Total Cost`; point plans
differ only in `Index Cond`. Scan direction, projection, and scan structure match.
Instrumented EXPLAIN also reports mode-dependent query identifiers outside the plan.
Param/Const bounds and arithmetic folding differ, so executor work can differ too.
The observed mode delta cannot be interpreted as pure planning CPU.

## Separate instrumented phase pass

The phase block enables `pg_stat_statements.track_planning` and runs only the
one-client range query. Keep these measurements separate from the clean controls.

| Mode | Calls and planning-span entries | Returned rows | Planning elapsed, µs/call | Inclusive execution elapsed, µs/call | Execution minus planning, µs/call |
| --- | --- | --- | --- | --- | --- |
| Generic | 166,780 | 16,678,000 | 0.667823 | 234.132722 | 233.464899 |
| Custom | 118,291 | 11,829,100 | 160.674790 | 375.193505 | 214.518715 |

These totals include warmup. Calls equal checked operations, and rows equal
100 times calls. The engine's execution timer includes planning. Do not add the
two timers. Planning occupies 0.2852% and 42.8245% of inclusive elapsed time,
respectively. These fractions are elapsed time with instrumentation overhead,
not CPU shares. `plans` counts timed planning-span entries, including cached-plan
selection, rather than actual rebuilds. Parse/Describe setup is outside these spans.
No phase result attributes time specifically to `plan_cost_estimate`.

## Export and audit

Run the read-only portable audit with Python 3 on a POSIX host from any directory.
The frozen parser imports the Unix `resource` module:

```sh
python3 /path/to/2026-11-02-planning/verify.py
```

The audit hashes exported files, independently regenerates the fixture digest,
parses all 84 invocation logs and resource records, recomputes summary statistics,
and reconciles phase counters and time arithmetic. It checks captured runtime
identities, settings, prepared counters, and recorded cleanup. It does not inspect
omitted executable bytes, rerun gates, or verify the current host's cleanup.
The completed continuation received a separate independent phase/cleanup audit.

[manifest.json](manifest.json) maps the portable block paths. `initial/` contains the
completed auto/generic block. `continuation/` contains auto/custom and the phase
block. Both retain their own exact scripts, inputs, runtime records, and cleanup.
`SHA256SUMS` covers every exported file except itself. Executables and fixture data
are omitted. Absolute paths inside captured records preserve the original cache
layout and are historical provenance, not portable links.

The first run completed auto/generic, then failed a safe port-bind check while
starting auto/custom on reused port 53320. The cause of that conflict is unproven.
[console.log](console.log) preserves the failure. The incomplete block contributes
no measurements. The continuation used distinct ports for the remaining blocks and
completed successfully. Both runs' cleanup records show engine exits of zero and
successful helper removal. A final live audit found all six owned PIDs absent,
ports 53320–53325 clear, and both exact helper IDs removed. Saved data is retained.

Frozen engine SHA-256 is `5cfc79043a50d0ac228bdac3c66deb5fcf0dd34d73add389bf2773237e51ffd1`,
from measured source `5b32046d`. Client SHA-256 is
`9f5b02222402fd5b09772088ce704054299339bb80723f1ef15fd915fb0b8e28`.
Prior build-provenance limitations remain. This warm read diagnostic establishes
neither behavior under sustained writes nor a PostgreSQL performance comparison.

The next experiment should retain CPU stacks across custom binding, deferred
planning, and repeated root-cost estimation. Those phases need attribution before
a larger estimate-reuse design or another local copying change is selected.
