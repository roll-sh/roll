# Draft choice: prune parallel alternatives at their existing owner

Compare the two completed designs in ../gather-lifecycle/once-design-{integrated,handoff}. Cross-judge review pending; no production implementation or timing has started.

Provisional scores (freshness, lifecycle, measured work, ownership, verification): integrated 4/4/3/4/4; handoff 3/4/4/2/3. Integrated keeps the selected serial observation at its existing position, and every decline resumes normal partial discovery. It retains serial tree construction, so the expected gain needs a new measurement. Handoff saves more success-path work but moves observations across Builder setup and adds one-shot identity and absence semantics.

The integrated design will borrow the existing ScanDesc for the same exact leaf in one Builder. It will mark the one eligible Project*→Scan root, cheaply decline total>=setup before metadata admission, and skip partial discovery only on a proven nonnegative enabled serial cost below setup. Public display builders never enable it. No cross-call state or descriptor cache.

Graft the handoff proposal’s exclusion-constraint refusal and positive-setup fallback checks. Keep boundary coverage for both custom and generic modes. Count actual serial descriptions and partial discovery in the same call using test-only counters. Fresh partial observations remain unchanged on decline. Tests alone may run the full oracle; do not add a racy debug assertion or new recorder to release.

A future trial must retain previous primary/control thresholds and add positive setup=1 fallback after observing both query totals exceed it and the real 200k plans qualify. No results from the rejected candidate can certify this design. Controls must count actual Gather queries accurately; prior low-CPU Gather control is only a coarse regression guard.

Cross-judge Claude Opus5.5 agrees integrated base. Required reconciliations: use existing hazard(filter,cat)==safe and max_workers>0; retain debug-only original serial costs and assert final root equality; promise only Gather sites/counts, since internal partial fields intentionally differ and this Builder is private to gathers. Add exclusion constraints refusal. Test full recorder fallback, actual per-call counts, custom+generic boundaries and positive-setup failures. A promising-but-refused control must be measured too. Debug numeric validation uses no extra model reads; the full-tree oracle lives in tests. No descriptor handoff.
