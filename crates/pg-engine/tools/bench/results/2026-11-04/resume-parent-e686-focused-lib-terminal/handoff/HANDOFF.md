# Encoding fixture PLAIN successor — UNRUN, TEST ONLY

Final candidate: e686696add25112c0d2ce33720bd84f0e04e21bf
Tree: 4baf09635451cf80e24c7670c4b85c67b5c137b5
Parent: cace40de4b9df2af42b08027138d9830d17abe9f
Local branch: feat/combined-index-validation
New backup: refs/heads/wip/combined-index-models-encoding-plain-r3-d62e3d3b
Exact remote SHA verified by ls-remote; see remote-shas.txt.

Single new commit, TEST ONLY: kept.rs +6/-2. The existing encoding test now sets
STORAGE PLAIN on all 16 payload attributes before INSERT, checks their catalog
storage settings, and checks that the leader's TLS encoding name equals LATIN1 or
UTF8 as requested. The revised comment states the heap compression assumption.
All worker assertions, bounded coordination, result/error assertions and settings
are unchanged. There are no production changes from cace.

The UTF8 fixture's approximately 3.9KB row crosses the heap TOAST threshold. Under
EXTENDED, PostgreSQL can inline-compress repeated text before index formation,
invalidating the old expected 54000. PLAIN prevents that heap compression and keeps
all values inline; the row fits a page. Each individual attribute is still below
the index compression threshold, while the whole UTF8 entry exceeds 2704. LATIN1
still fits. These remain unrun definitions and static reasoning, not a captured
PG size, layout or outcome oracle. No expected golden was manufactured.

Read the full cached combined-encoding-proof-review-r3-result.json. Its production
worker guard, ready-first observer, tuple annotation and corrected-red scope
judgements are static only. The MEDIUM fixture issue is corrected here without
changing the production guard or expanding observer architecture.

The worker guard installs the catalog DbDefault, which contains more than the
encoding: it also aligns database ctype, default collation, and jsonb comparison's
default_unless_bytewise context with the leader. This is existing production code
from 9278, unchanged here; it is not claimed to be a fully tested locale fix.
Measured-worker encoding coverage remains held. Sibling parscan workers and the
binder's user-cast/operator-family TLS are held separately. No broader propagation
or formatting repair is included. Original shared-validator ordering, stats fixes,
fast path and exact two-worker/actual modeled-path proof requirements remain.

Corrected red remains immutable:
  b56ed0fb4c9a6113cfef09504e2b695a1cf17b70
  refs/heads/wip/combined-index-models-red-proof-r2-d62e3d3b
No new red commit or ref was needed for this encoding-only fixture correction.
The red lacks the encoding test and production guard; filtering the encoding test
there runs zero tests and proves nothing. Historical b43/e25/cace objects, refs and
manifests are preserved. Prior reports labelling b56 unrun predate parent execution.

Updated external evidence (parent report, not independently rerun here):
- b56's original-HIGH behavioral red exited 101 with two real workers, 6000 modeled
  returns and zero validations; parent reports seal 4bf103ca. That evidence is for
  that exact red/proof, not for the encoding fixture or complete candidate acceptance.
- The first intended cace attempt was invalid because Cargo reused the red binary;
  parent reports seal 834a37fa. It is not cace behavior evidence.
- Parent's corrected runner refreshes lib.rs mtime in its immutable cache snapshot
  and requires compilation from the intended snapshot plus a changed binary. This
  agent did not touch that snapshot, green-src, lib.rs, targets or the running job.
- No later valid cace outcome was provided with this request, so none is claimed.

Static checks only: diff --check passes. Only one tracked file changed, wholly
inside the existing cfg(test) module; its production prefix and every other blob
match cace. The new manifest verifies all 39 committed changed-file blobs against
the clean isolated worktree. Old manifests were independently verified against
their immutable objects and retained unchanged; their artifact hashes are recorded.
Six original/current remote refs were checked by exact ls-remote SHA.

Artifacts in this directory: candidate-manifest.json, verification.json,
preserved-artifact-hashes.json, remote-shas.txt, from-cace.patch, diffstat.txt and
verify.py. Earlier detailed guard/proof/statistics handoff remains untouched at
../encoding-boundary-successor/HANDOFF.md; this document supersedes its UTF8 fixture
assumption and its historical b56 execution status only.

Parent execution proposal, never run here, using its existing resource wrapper and
intended-source/binary checks:

  cargo test -p pg-engine --lib combined_index_worker_database_encoding -- --nocapture --test-threads=1

Retain full trace and confirm exactly one test ran from e686696a's bytes. A separate
PG18 capture of the PLAIN fixture and a separately owned encoding-specific behavioral
red remain pending. The original-HIGH proof and other regression gates are separately
parent-owned. There is no source, performance or footprint acceptance.

No builds/tests/cargo/rustfmt/services/containers/new targets or new worktrees were
used here. No owner/shared sources, indexes, branches, parent cache snapshots or
reserved targets were edited. No Load or measured-parbuild repair was integrated:
the measured raw/detoasted heap sizing root TOAST gap and raw Load failure of the
older 8300 fixture remain held. All prior stats/minimum/TID PG-oracle gaps, GIN
limitations and P6–P12/full verification obligations remain disclosed.
