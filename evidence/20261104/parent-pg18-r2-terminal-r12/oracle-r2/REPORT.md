R2 completed the single allowed PG-only job; four estimated-engine TID comparisons differ. No fixture was retried or edited.

| Reference (serial0 and setting2 unless stated) | Actual PostgreSQL18.6 result |
|---|---|
| Minimum ASC | 54000; row2896 >2704; DETAIL tuple(3,45), key-20/payload2880 |
| Minimum DESC | 54000; row2896 >2704; DETAIL tuple(1,115), key20/payload2880 |
| Include2800 | 54000; row2816 >2704; DETAIL tuple(32,80) |
| Include8300; Duplicate8300 | 54000; requires8320 bytes, maximum8191; formation error wins |
| Duplicate2800; Duplicate0 | 23505; could not create unique index ci_i; Key(k)=(1) is duplicated |
| Stats2800/8300 | Captured SQLSTATEs and six counters match the source's asserted behavioral contract (details below) |
| PLAIN LATIN1 | CREATE INDEX succeeds; p0 octets120/chars120; all16 storage p |
| PLAIN UTF8 | 54000; row3920 >2704, DETAIL tuple(0,1); p0 octets240/chars120; all16 storage p |
| Frozen generic parse/bind_named | Six b='1'; generic_plans1/custom_plans5; actual psql exit0 |
| Frozen parallel parse/bind_named | Each named statement returns a=3000000 once; each planned_delta2/launched_delta2; both EXPLAIN Gather/Workers Planned2; actual psql exit0 |

The build error SQLSTATE/message/DETAIL/HINT matches the cached physical-engine trace for all14 cases. Comparison with cached estimated-engine traces retains four DETAIL mismatches:

| Case | PG actual TID | Cached estimated-engine TID | Settings |
|---|---|---|---|
| Minimum ASC | (3,45) | (1906703067,17643) | 0 and2 |
| Include2800 | (32,80) | (1904608731,56300) | 0 and2 |

The other compared fields match. These differences remain failed comparisons in report.json; no expectations were changed. This report makes no claim that estimated synthetic TIDs should equal physical PostgreSQL TIDs, and it does not close broad PG parity.

Counters are [n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum]. Both lengths produced identical full sequences:

| Boundary | Counters | Delta for statement | Visible row count |
|---|---|---|---|
| Initial small row | [1,0,0,1,0,1] | — | 1 |
| Single failed INSERT | [2,0,0,1,1,2] | [1,0,0,0,1,1] | 1 |
| Failed two-row INSERT | [4,0,0,1,3,4] | [2,0,0,0,2,2] | 1 |
| Successful small INSERT | [5,0,0,2,3,5] | [1,0,0,1,0,1] | 2 |
| Failed UPDATE | [5,1,0,2,4,5] | [0,1,0,0,1,0] | 2 |
| Failed ON CONFLICT DO UPDATE | [5,2,0,2,5,5] | [0,1,0,0,1,0] | 2 |
| Failed no-conflict INSERT DO NOTHING | [6,2,0,2,6,6] | [1,0,0,0,1,1] | 2 |

Expected failing statements actually report54000; only successful small INSERT reports00000. Error text is row2816 >2704 for2800 and requires8320 >8191 for8300. Each observation used force flush and clear snapshot at psql command/autocommit boundaries. All visible retained payloads remain 'small'. UPDATE/upsert INSERT counters remain unchanged; their full six-counter deltas above are PG observations beyond the source's two-counter negative assertions.

Version SELECT over TCP verified PostgreSQL18.6/180006; client verified psql(PostgreSQL)18.6. Every psql command used -h127.0.0.1. The final postmaster listens on127.0.0.1; temporary init postmaster had no TCP listener. Original20 fixtures/admin were copied byte-for-byte, as were both frozen controls with their requested hashes. The planner controls each used exactly one psql18 connection for the entire unchanged file, -X -v ON_ERROR_STOP=1, in separate clean template0/C/UTF8 databases. No SQL PREPARE substitute.

Planner preflight: max_parallel_workers8, max_worker_processes8, max_parallel_workers_per_gather2; no active parallel workers. Defaults: parallel_setup_cost1000, parallel_tuple_cost0.1, seq_page_cost1, random_page_cost4, plan_cache_mode auto, track_counts on, track_activities on, stats_fetch_consistency cache. Raw settings include all requested settings. The frozen controls' two launched_delta2 observations independently establish actual planner worker launches. Original CREATE INDEX setting2 by itself is not worker proof; worker-log-excerpts.txt additionally preserves explicit PG background-worker start records and index requests without conflating requests with starts.

Launch/job metadata:
- Evidence job ID: combined-pg18-oracles-r2.
- Existing delegated task ID decoded from current backing-thread identity: command:mcp:9e5a6feb-d435-4514-ac9d-07c1b03b444b:delegate-task:pg-resume-20261104-combined-pg18-oracles-r1. No new delegated task/child was created.
- Exact backing thread ID: thread:delegated-task:command%3Amcp%3A9e5a6feb-d435-4514-ac9d-07c1b03b444b%3Adelegate-task%3Apg-resume-20261104-combined-pg18-oracles-r1.
- Detached wrapper PID375247; runner PID375269; bounded process group375268.
- UTC 2026-11-04T20:30:48.251624+00:00 through 2026-11-04T20:31:07.473864+00:00; elapsed19.222295s.
- Runner actual exit0; terminal HEAVY-EXIT0. Launcher exit0 only indicates detached launch acceptance. All20 fixture psql processes and2 planner controls exited0; expected SQL errors remain preserved as SQLSTATEs and stderr.
- Container t3-d4020b98-combined-pg18-oracles-r2; IDaf69410964739cc260180a9231ccc4bf0e2a5b66ada669de07a72d169fa56d7b.
- Image sha256:898c60cb390d1085bc97bdd6a1ae968757421df394b071848b52b1c09c89d6a5; --pull=never; no alternate image/version.
- 512MiB RAM+swap cap; cpus2; cpuset4-7,22-25; networknone; no published ports; Mounts=[] (no Docker volumes/binds); image VOLUME overridden by empty1MiB tmpfs; ephemeral PGDATA=/tmp/pg18-reference-data in container writable layer.
- shared_buffers16MB; max_connections12; debug1 server logs; launcher taskset4-7,22-25; unchanged heavy-bg/heavy, nice19, global slots4/jobs2 unchanged. CPU/affinity/cgroup/memory-event samples and full Docker inspect preserved.
- Process-group timeout580s/kill-after8s; internal550s deadline; job completed in19.223s. Free disk before job238172377088 bytes >30GiB. Health preserved; no performance claim.
- Authorized synchronization recreation only: /home/meszmate/roll/.claude/worktrees/agent-a3512214927146502/target and its empty .heavy.lock. Observed absent before launch, present afterward with no other entries. No inference about disappearance; no Cargo invocation, CARGO_TARGET_DIR change, build artifacts, replacement target or cleanup.
- Finally removal actual exit0; exact-name container absence actual exit0 and empty stdout. Other containers, including a171-pg, were untouched.

R1 preservation: all93 original manifest entries verified before and after; all bytes unchanged, including its terminal and manifest. Wrappers also match copies after R2.

Artifacts:
- terminal.log: full combined raw terminal, SHA256 0daf84f22f162cd5d8a5da63d4648406b466d7e2f9166fccc0306b6363207bd1.
- commands.json plus numbered .command.json/.stdout/.stderr: all44 in-job commands, exact argv/stdin hashes/times/actual exits, including cleanup. Preflight, launcher, and synchronization inspections are also separately recorded.
- report.json: complete parsed observations,311 post-capture comparisons with4 TID mismatches retained; no wrapper-exit-as-parity assertion.
- resource-identity.json; launcher.json; run-result.json; synchronization-metadata.json; wrapper-postflight-hashes.json.
- copied-sql-sha256.json: all23 frozen original/planner SQL file hashes, file SHA256 078735fa800e76f97bd150b7aa585077c2f1028f88546135c28c6e2b652fecae; original sql-sha256.json retained unchanged.
- source-runner-hashes.json: kept.rs and parent raw engine log identities; source-fixtures.txt: numbered fixture source.
- MANIFEST.sha256 seals all R2 evidence except itself.

No engine execution/build/test/benchmark, source/ref/index/worktree mutation, additional service, crash/SSI/storage probe, children or push. Parent source acceptance, performance, broad PG18 parity and TOAST closure remain held.
