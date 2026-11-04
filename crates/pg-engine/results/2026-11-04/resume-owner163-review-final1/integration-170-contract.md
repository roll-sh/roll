Read-only integration assessment, 2026-11-04.

Sources: owned16337e9896225915add74cb51e0d966765cdf01d8b4 (pinned gate active);
owned1703bb04519be5a02bc6a041395a3a122a644a994d8 (read-only).
No170/169 source or target changes performed.

170 Plan::row has self.model early return at parbuild.rs110-119. Existing163cf27
parallel INDEX_SIZE_MASK/oversized tracking is in the measured path. A textual merge
alone therefore fails to validate modeled-table worker entries.

Integration contract sent to execution_models owner and parent:
- Decode the needed row and key values once, before branching on model.
- Size key values plus INCLUDE using ix_types/fmt before either branch can return.
- Reject INDEX_SIZE_MASK immediately; track the earliest index-entry key above BT_MAX_ITEM
  per Gathered, for both modeled and measured paths.
- Leader combines all Gathered oversize candidates, checks uniqueness, then raises the
  earliest btree oversize error before Modelled return or entry-run installation.
- The model/index-entry key uses key values only; INCLUDE values are for tuple size.
- stores_entries gates storage writes, including put_runs, independently from sizing and
  uniqueness. Worker eligibility must retain the163policy and170modeled eligibility.

Required combined-source acceptance proof, not yet run: modeled table with parallel_workers=2,
maintenance_work_mem=96MB and existing global jobs/slots unchanged; serial-vs-two-worker
oversized key and INCLUDE cases, both8191formation and2704limits plus unique precedence;
prove actual modeled worker path via existing instrumentation/progress rather than settings
alone. Compare PG18.6 outcomes. Parent owns actual combined integration and final gates.
