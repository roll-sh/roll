# Recovery checkpoint r3

The shared branch contains the verified five-file candidate 6493cf49 as eb450f4e. Runtime source, Cargo files and test SQL/Rust/goldens in the checked scope match tested owner HEAD 3997cf00; the scoped difference is STATUS.md. Outside the scope, HANDOFF.md, the canonical audit-tap.py and evidence files differ. This checkpoint changes evidence only.

Corrected regression and isolation preserve their declared baselines. Separate first strict dump passed 14,817/0 and complete TAP plans. Earlier failed v3 and v4 attempts remain sealed as failures. Full product acceptance is false. The copied verification-readiness.json is an immutable historical owner record; acceptance-scope.json clarifies that final combined pg-engine checks and the Postgres service-stack compatibility gate are separate requirements. pnpm test:pg --force does not exercise pg-engine. Neither final combined gate has run.

The unlaunched service runner now rejects failure/compiler/panic/skip markers, requires a positive fully passing e2e.rs summary, and exits on failed network preflight. The original proof preserves 16 exact saved-snippet controls and two constructed original-network baselines. The successor full-service-guard-proof-v2 exercises the whole runner with fake git, Docker, network-preflight and pnpm dependencies (only W/V path assignments adapted): all 15 controls pass, including actual network_rc capture. No real services, compilers or source suites ran in these proofs.

Terminal independent review summaries and task results are saved beside launch receipts. trail-review-r4-result.md records two medium findings and lower-severity notes; review-disposition.md describes each correction. Earlier source findings remain open where recorded; passing checks do not waive them. The decision excerpt is append-only history and its timestamps are recording times, including explicitly disclosed retrospective entries.

Direct parent audit of storage final seal25d8cd07 verifies all102 payload blobs, their closed membership, hashes and git object IDs, plus2367 source-file hashes against79746f31, all14 recorded command exits and expected test summaries. This establishes the checked evidence scope, not performance or combined acceptance.

Validator-controls includes the full parser, saved inputs/outputs and version matcher controls. To replay without modifying sealed evidence, run `python3 replay-validator-controls.py /tmp/a-new-nonexistent-output-directory`. It writes only to that new directory. validator-replay-results contains the parent's10-case replay.

To rerun either service proof, copy ONLY its prove.py and input scripts into a new scratch directory, then run python3 there. For the original proof copy original.sh and repaired.sh; for v2 copy hardened.sh. Do not copy generated case directories. Do not execute saved command.sh files directly: their V paths point to the original evidence directories. Scratch execution changes timestamps and output paths; preserve the sealed copies.

Other code remains held: index review fixes have active gates; catalog extra tests prove restrict but leave post-DDL count coverage open; OLTP hidden domain effects now have a concrete red and scoped fix validation; scan149's endpoint bug has a deterministic cold/hot red, while historical6003 attribution remains unproven. Executor/model static findings are saved and routed; no such source has landed. Storage performance has begun on verified dist builds; an initial TIME_WAIT guard failure is preserved, a distinct continuation retains valid baseline0, and no comparison verdict exists. Latest owner job outcomes continue asynchronously beyond this snapshot.

SHA256SUMS hashes every file except itself. Earlier checkpoint directories are unchanged.
