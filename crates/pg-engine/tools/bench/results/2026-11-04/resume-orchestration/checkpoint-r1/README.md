# Recovery checkpoint

This receipt preserves the integration decisions and confirmed remote checkpoints while owner checks continue. No recovered engine candidate is accepted by this receipt.

- Shared head `d599cf05` contains the canonical TAP auditor plus previously pushed evidence. The parent replayed 16 controlled and real fixtures before integrating the auditor.
- Verification candidate `6493cf49`, tested at `3997cf00`, completed build, release, debug, Clippy, PostgreSQL unit and difftest compilation checks. Release and debug each report 451 passed, 18 ignored, zero failed. The copied audit retains the failed first parity attempt. A bounded repaired retry is pending; the full combined service gate has not run.
- OLTP candidate `baeee87a` remains held for the prepared-lock and scroll-cursor findings. Its completed old-source test results do not validate the pending fixes. The low-priority litplan finding concerns search-path allocation in `lit_bind`/`lit_replan`. The owner's PostgreSQL recording command contradicts the review's claim that its golden was engine-recorded; fresh reference comparison remains required.
- Storage candidate `fadf2751` failed its new proof suite: two passed and two failed. A second static review also found a close-time manifest/view insertion-index race and excessive rewriting from staging distant L1 files. The owner is diagnosing tests before further edits; a per-file close rewrite was requested.
- Catalog candidate `0cd26bfd` has an unproven statement-catalog visibility concern under volatile DDL; an owned reproduction is pending. Executor, scan and model exact-source checks are running or queued.

Performance and footprint gates remain open. This checkpoint makes no claim that roll is faster than PostgreSQL. Remote identities are direct `ls-remote` observations; local status summaries and owner reports are explicitly scoped above. The canonical trail excerpt is append-only history, so later rows may supersede earlier claims.
