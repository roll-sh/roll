# TAP audit contract

`audit-tap.py pg-dump <wrapper-log> <raw-directory>` requires four PostgreSQL18.6
suites with exact plans96/14609/4/108, ordered numbered results1..N, zero failed,
no skips/TODOs/bailouts, and matching successful per-suite wrapper summaries.
A Rust test-success line alone has no authority. Missing or truncated raw data fails.
`audit-tap.py tap <file> --expected N` checks one numbered TAP stream.

Proofs: tap-audit-proof.json records twelve controls, real14817/14817 output,
and rejection of real166 output7020passed/7797failed despite Rustexit0.
Raw inputs are checkpointed in tap-proof-raw/{negative166,historical162pass}/;
run pg-dump with wrapper.log and its containing directory. Negative166 must exit1;
historical162pass must exit0. Every checkpoint file has a SHA256 manifest entry.
The negative166 issue is a missing Docker bind mount/workdir (OCI chdir), not a
source-regression finding. Historical162pass is older source, not a fresh candidate gate.

The queued current gate reuses storage_correctness/check-tap.py's total-count gate,
then this stronger plan/numbering checker. Its dump workdir is under the existing
client bind mount ~/.cache/a162/pgdump. Actual dockerexec-w/preflight confirms writable
workdir and pg_dump/pg_restore/psql18.6; it repeats before the long current run.
No extra service instance was launched. Child process exit codes are not separately
persisted by the legacy Rust harness; this checker certifies complete TAP output,
while the owning fail-fast runner also records the cargo command's exit.
