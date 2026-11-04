Read-only localization of owner149's preserved transfer failure

Candidate production source: 2a854a48074dbd3317807f634a773f29540b2673,
tree 7b41c62e7ca291f3f349ec68838dff1ab5f3b34d; clean during localization.
Comparison: f0563cd8687f155ec8d31cf47d8fa891a11d0c72.
Only four Rust files differ: exec/scan.rs, storage/mod.rs, storage/row.rs,
txn/mod.rs. No DML/SSI bookkeeping changes between these sources.

Concrete defect identified statically

- New ScanNode::fetch_kept (scan.rs:3434) uses end = k + [0]. The comment
  assumes prefix-free user keys make that a sufficient bound.
- Txn::scan_snap -> Txn::lower -> Store::scan_raw sends that bound unchanged
  to Cursor::Mem::fill / Cursor::Level::head. These compare PHYSICAL keys.
- mvcc_key (storage/mod.rs:67) stores k + (!ts).to_be_bytes(). Every physical
  version of k sorts after the shorter k + [0] bound, including a timestamp
  whose suffix begins with zero. The cold merge therefore excludes the key.
- Hot::Chains::visible (hot.rs:117) compares USER keys and includes k in the
  same range. Promotion happens after eight noted scans (including partial
  ranges), so the bug can disappear when a range becomes hot. Own-write
  overlays also compare user keys and can supply a row cold storage misses.
- Existing prefix_end(k) is an appropriate endpoint for prefix-free user
  keys and all their physical versions. fetch_kept already checks rk == k.
  The smallest proposed production fix is that endpoint change alone.

Transfer correlation and first likely wrong state

tests/app_concurrency.rs:225-300 creates six accounts at 1000, then twelve
sessions each commit 25 transfers. Each attempt sends BEGIN SERIALIZABLE,
SUM, debit UPDATE, credit UPDATE, ledger INSERT, COMMIT as one batch. Only
40001/40P01 retry, after explicit ROLLBACK. batch_execute discards command
row counts. It does not assert that either UPDATE touched its account.

An account equality read can use the secondary primary-key index (default
format stores rows by hidden rowid), fetching one untracked numeric row via
fetch_stored -> new fetch_kept. Exact physical plan was not logged in the
historical failure; a minimal SQL probe will assert this Index Scan.

First likely wrong state: existing committed account key -> None in a cold
fetch_kept. next_indexed discards absent rows. UpdateNode::next ends when
its source returns None (dml.rs:4846), before the per-row lock and concurrent
version check (dml.rs:4909, scan.rs:4860-4914). UPDATE 0 is successful, and
the corresponding ledger row can commit with only the credit/debit applied.
The full-table SUM at each attempt makes hot promotion sensitive to timing.
This is a mechanism consistent with +3, not proof that it caused that run.

Preserved observed evidence

- Candidate debug log: exit101; lib184 passed/2 ignored; app_concurrency
  5 passed/1 failed/1 ignored. Final assertion at line283: (6003,300),
  expected (6000,300). The later per-account ledger check never ran.
- Source before/after matched clean candidate2a854a48. Raw log hash:
  c65acb1b37d7b540d0ce18912e91ea6d29245398204a8a8cef310aa6cb08eb53.
- Baselinef0563cd8: one focused transfer trial passed, exit0; 1 passed,
  6 filtered. This is a limited comparison, not attribution or clearance.
- Candidate failure's executed binary hash was not captured before baseline
  compilation overwrote the artifact. Retained baseline identity is included;
  it cannot establish the failed candidate binary's exact identity retroactively.

Smallest distinguishing runtime probe (subsequently authorized)

Fresh Store, one prefix-free relation key, known committed value/timestamp,
pinned snapshot: compare get_version_raw, narrow k+[0] scan, prefix_end(k)
scan before hot promotion. Record Scan.rows None to prove merging path.
Run eight whole-prefix scans, repeat narrow scan, assert Scan.rows Some to
prove hot-chain path; assert values and unchanged committed timestamp.
Gather all results before the final equality assertion so red retains both
cold and hot observations. No concurrency/SSI fuzz suite is needed.

Optional minimal SQL probe: force a secondary primary-key Index Scan over
one numeric account, record cold UPDATE count/balance, promote via eight
SUM scans, record hot UPDATE count/balance. Expected behavior is one row
changed in both states. Keep the preserved failure and frozen gates unchanged.

The byte-order witness is a cheap Python source-formula check, exit0 for
four timestamp cases; it is explicitly not execution of the Rust Store.
Runtime red/green commands, source/test patches, logs and binary identities
will be exported separately. No new acceptance or performance claim.
