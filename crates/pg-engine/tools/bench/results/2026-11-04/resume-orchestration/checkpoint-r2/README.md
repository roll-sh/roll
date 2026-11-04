# Backup audit follow-up

The independent Claude Opus 5.5 trail audit found checkpoint-r1 accurate at its observation time. It returned after commit `58ddbac7` had landed; that checkpoint was not reviewed before its push.

The audit found an evidence gap: filesystem manifests listed raw `.log` files that git ignored. New immutable sibling supplements now contain those files. The parent independently checked all 228 payloads against committed bytes, SHA256, sizes and git object ids, checked each changed-path set, confirmed the 95 previously missing blobs were absent from the old trees, and verified all four remote refs. The earlier artifact counts did not establish complete raw-log backup. Original bundles remain unchanged.

The audit also required independent checks of test and validator changes after failures. Storage review r3 of `79746f31` independently validated the fixed-width key fixture and compression-policy assertions; five offline compression controls are preserved in the owner169 supplement. The repaired shell phase passed all eight positive and negative controls. Separate array-case validator controls are included in the next bounded verification continuation and remain pending at this checkpoint.

Verification still failed its full regression threshold: 49,868 of 51,150 units matched, below the required 51,148. A source-fixture symlink escaped the engine's allowed file root and caused COPY refusals. This explains observed setup failures, not necessarily every mismatch. One corrected continuation is authorized only after exact old-root rejection and corrected-root/PG row-count preflights. Already-passed pair, smoke and binary-array workloads will not be repeated. Isolation and pg_dump remain pending.

Catalog166 owns the narrow captured-catalog context API in its own worktree, coordinated with executor167. Its exact-source checks and independent review are pending. Storage, OLTP, index, model, executor and scan candidates remain unaccepted pending their required checks; the inherited late-writer close-compatibility gap is disclosed rather than fixed. No timing or overall PostgreSQL superiority is claimed.
