## Catalog round10: catalog generation and snapshot correction (agent166,2026-11-04)

The batch narrows catalog/statistics generation and defers broad pg_class/pg_attribute
rows until first consumption. HANDOFF's Catalog round10 describes current APIs.
The initial lazy implementation widened visibility when a FULL JOIN built an inner
volatile DDL call before consuming the outer catalog scan. PostgreSQL18.6 hides the
new relation/attribute in the opened scan and shows it next statement; the new golden
records that result and the correction retains the opened catalog version.

No new performance acceptance is claimed. The original loaded-host timing/profiling
outputs remain historical evidence. Broad relkind filters still build keys before
the WHERE_MAX8192 and quarter-of-catalog checks, and per-relation generation needs
10,000 tables plus composite types in its bounded latency/CPU/footprint comparison.
That comparison is HELD; shared service verification is parent-owned. Earlier failed
pg_dump TAP and E2E outputs stay failed even where a Rust wrapper returned0.
