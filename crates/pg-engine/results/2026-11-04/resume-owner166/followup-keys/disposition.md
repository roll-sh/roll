The INNER probe has the same Hash Join with a direct pg_class outer scan and volatile
Result under the inner Hash on PostgreSQL 18.6 and the engine. Both hide the new relation
and show it next statement. The integration gate will require a positive per-thread
rel_rows_generated delta to prove the outer scan uses restricted generation.

Neither bounded count probe proves catalog retention after build-side DDL. In p1 the engine
commutes the FULL JOIN and builds the count aggregate before running the function. In p2,
PostgreSQL uses Hash Semi Join but the engine uses a hashed SubPlan filter above count.
p2's strict plan-shape assertion FAILED with exit 1. Its raw wrapper remains failed even
though both query result transcripts are 1 and true. No further count probe is planned.
A counter test can establish count_rest avoids whole catalog generation, with that limit.

The production fix captures the catalog when the node opens. It does not change the
preexisting difference between node-open and statement-start catalog visibility for a
scan constructed after nested DDL. These tests do not establish statement-start semantics.

Gate1 on 55ddc51e FAILED with cargo exit 101: the result was false, but the positive
restricted-generation assertion failed. Release and Clippy did not run. The attempted
zero-cost setup was based on an incorrect premise. The engine uses hardcoded planner CPU
cost constants, so setting the GUCs to zero does not zero its costs. Its selected inner
hash join reads the first outer row before an observable build, materializing the catalog.
A controlled-plan test will assert that metadata and change only outer_first on a clone.
The failing assertion is retained for the controlled path, not weakened on the natural one.
