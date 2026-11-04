Reference capture failed before fixtures; no PostgreSQL parity result.

The only allowed job ran 2026-11-04 20:13:05–20:13:08 UTC (3.945 seconds).
pg_isready accepted the image entrypoint temporary initialization postmaster.
SELECT version() then exited 2: socket /var/run/postgresql/.s.PGSQL.5432 absent,
during normal temporary-to-final-postmaster startup. No SQL fixture was executed.
This is a harness readiness error, not an observed fixture outcome.
No second container/job was started.

| Requested reference | Outcome |
|---|---|
| Minimum ASC/DESC, serial0/setting2; Include/Duplicate precedence | Not measured |
| Stats 2800/8300, six behavioral statements | Not measured |
| PLAIN LATIN1/UTF8, serial0/setting2 | Not measured |

Image: sha256:898c60cb390d1085bc97bdd6a1ae968757421df394b071848b52b1c09c89d6a5.
Image PG_VERSION=18.6 and final server startup log reports 18.6; SELECT verification did not complete.
Container: t3-d4020b98-combined-pg18-oracles-r1, id 70386cc43ec065bf6dde21821bdd060cdaf27abdb4c0270f6ed38a039a767f69.
512MiB memory+swap cap, 2 CPU quota, cpuset4-7,22-25, shared_buffers16MB; no published ports, networknone, no Docker volumes.
An empty 1MiB tmpfs overrides the image VOLUME; PGDATA is on the removable container layer under /tmp.
Launcher taskset4-7,22-25; existing heavy-bg.sh/heavy.sh, nice19, shared jobs2/slots4 unchanged.
Process-group timeout580s with kill-after8s and internal550s deadline.
Free disk before job223109107712B (>30GiB); no performance claim.

Runner actual exit1; HEAVY-EXIT1; launcher actual exit0 only means detached launch accepted.
Finally docker rm -f actual exit0; exact-name docker ps -a empty, actual exit0.
Other containers were left untouched. No engine build/test/probe, source edits, new Cargo target, or children.

Evidence: terminal.log has the full combined terminal; commands.json and numbered command.json/stdout/stderr files record exact commands and actual exits.
source-runner-hashes.json includes sealed kept.rs, parent raw candidate2.log, and runner wrapper hashes; source-fixtures.txt has numbered fixture lines.
sql-sha256.json hashes all prepared but UNEXECUTED SQL; launcher.json hashes run.py and prepare.py.
report.json preserves container host configuration and mounts. MANIFEST.sha256 seals every evidence file except itself.
Parent acceptance, performance, broad PG18 parity, TID/error-size parity, stats parity and encoding parity remain held.
