Catalog166 bounded follow-up, tested source 0925007d92106d171d75a25d2cc0dd123e1ddd3d

The natural INNER SQL has a direct pg_class outer scan and a volatile function under the
inner Hash on PostgreSQL 18.6 and the engine. Both return false for the new relation and
show it next statement. These matching plan shapes alone do not prove identical evaluation
order. Gate1 on 55ddc51e FAILED with cargo exit 101 because restricted generation stayed
zero. Release and Clippy did not run in that gate. Its raw log and source hashes remain.

The initial zero-cost premise was wrong for the engine. Its planner CPU costs are hardcoded
constants, so setting the GUCs to zero does not zero its path costs. The naturally selected
Hash Join has outer_first=true. With an observable function on the build side, the engine
reads the first outer row before the build, materializing the catalog. This is not a
cold-cache setup failure. No cache clearing or production change was used as a repair.

The final Rust test binds that same SQL through public APIs and asserts Hash Join, a direct
pg_class outer scan, an observable build, and outer_first=true. It clones the plan and
changes only outer_first to false. The test asserts that the catalog rows are not cached,
executes the controlled plan, requires the PostgreSQL-recorded false result, and retains
the positive restricted-generation assertion. Debug and release each observe exactly one
restricted generation. This proves the deferred node contract under controlled build-first
execution. It is not proof that the natural engine plan selects this execution order.

Count coverage remains limited. In p1 the engine commutes the FULL JOIN and builds the
count aggregate before the function runs. In p2 PostgreSQL uses Hash Semi Join, but the
engine keeps a hashed SubPlan filter above count. The p2 plan-shape assertion FAILED with
exit 1 and remains failed. Both actual result transcripts are 1 and true. The final test
observes zero whole-catalog generations for the plain count query. Cached rows can also
produce that observation, so it does not prove a count_rest call or catalog retention when
DDL precedes the count. No third count probe was run.

The fix captures the catalog when a node opens. It does not change the preexisting
difference between node-open and statement-start visibility for a scan constructed after
nested DDL. These tests do not establish statement-start semantics.

Gate2 passes all three declared steps with source hashes and HEAD unchanged. Debug and
release each pass the controlled test and four a166 files in process, over the wire, and
in shuffled order. Clippy returns zero with no added diagnostic locations. Production and
Cargo files are unchanged from 256e4d04. No full service, TAP, or performance run was added.
Parent combined service verification and performance/footprint acceptance remain pending.
