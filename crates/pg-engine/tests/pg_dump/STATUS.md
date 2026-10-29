# pg_dump / pg_restore / pg_dumpall / psql: status

The harness is `tests/pg_dump.rs` (three `#[ignore]`d tests); the stand-ins for PostgreSQL's
TAP modules are in `tests/pg_dump/perl/`, psql's command list in `tests/pg_dump/psql_meta.sql`.
Every client program is PostgreSQL 18.6's own, run in a container of the reference image on
the host network (`PGDUMP_CLIENT`), so dumps, restores and psql output on both sides come from
the same binaries.

## How to run

```sh
D=~/.cache/x                     # on disk: /tmp is tmpfs
docker run -d --name x-client --network host --user $(id -u):$(id -g) -v $D:$D \
  --entrypoint sleep <the pinned reference image> infinity
# 1. the regression suite into a database kept on both sides (the reference: a cluster of
#    your own, since pg_dumpall and the TAP tests read and change globals)
PGREGRESS_KEEP=1 PGREGRESS_DIR=$D/pgregress PGREGRESS_DB=regression_x PGREGRESS_CONTAINER=x-ref \
  PGREGRESS_ENGINE_CONTAINER=x-client ... \
  cargo test -p pg-engine --test pg_regress -- pg_regress --exact --ignored --nocapture
# 2. the round trips over it, psql's backslash commands, the TAP tests
export PGDUMP_DIR=$D/pgdump PGDUMP_CLIENT=x-client PGDUMP_REF_PORT=<x-ref's port> \
  PGREGRESS_DIR=$D/pgregress PGREGRESS_DB=regression_x
cargo test -p pg-engine --test pg_dump -- --ignored --nocapture pg_dump_roundtrip
cargo test -p pg-engine --test pg_dump -- --ignored --nocapture pg_psql_meta
cargo test -p pg-engine --test pg_dump -- --ignored --nocapture pg_dump_tap
```

- `pg_dump_roundtrip` dumps both regression databases in every format (plain, custom,
  directory, directory with `-j 4`, tar) and with `--schema-only`, `--data-only`, `--inserts`,
  `--column-inserts --rows-per-insert`, `--no-owner --no-privileges`, `-n`/`-N`, `-t`,
  `--clean --if-exists --create`, `--lock-wait-timeout --serializable-deferrable`; a non-plain
  archive is read back with `pg_restore -f -`. Dumps are compared entry by entry (`-- Name:`
  sections), normalizing only what PostgreSQL itself varies (the `\restrict` key, large
  objects' oids). Then each side's custom dump is restored with `pg_restore` and its plain
  dump with `psql` into a fresh database on the other side, the restore's errors counted, and
  the restored database dumped again and compared with the target side's own dump. Last,
  `pg_dumpall --globals-only / --roles-only / --tablespaces-only` on both sides. It writes
  `REPORT.md` to `PGDUMP_REPORT_DIR` and the per-run diffs to `$PGDUMP_DIR/diffs`.
- `pg_psql_meta` runs `psql_meta.sql` (the \d family over every object kind with + and S,
  \l, \dn, \df*, \dT, \dp/\z, \dx, \des/\deu/\dew/\det, \dF*, \dA*, \dconfig, \do, \sf, \sv,
  \ef/\ev with `PSQL_EDITOR=cat`, \gdesc, \crosstabview, \watch, \copy, \lo_list,
  ECHO_HIDDEN) on both regression databases and compares the output per script line.
- `pg_dump_tap` runs PostgreSQL's own pg_dump TAP files (`src/bin/pg_dump/t`, REL_18_6)
  unmodified against a fresh engine each. `003_pg_dump_with_server.pl` needs a test extension
  built into the server and `010_dump_connstr.pl` clusters made by initdb with other bootstrap
  superusers; neither runs. The reference (a fresh cluster) passes every test of 002.

## Scores

| measure | before (2d82227) | round 17 (agent 37) | round 19 (agent 54) |
|---|---|---|---|
| 002_pg_dump.pl TAP checks | 32 / 14,609 (pg_dump could not start) | 14,604 / 14,609 | 14,609 / 14,609 (the last, the locale provider, with agent 60's database locales) |
| 001_basic.pl, 005_pg_dump_filterfile.pl | not run | 96 / 96, 108 / 108 | 96 / 96, 108 / 108 |
| 004_pg_dump_parallel.pl (parallel dump and restore) | 2 / 4 | 4 / 4 | 4 / 4 |
| regression database dumps (pg_dump runs at all) | fails | every format and option dumps | every format and option dumps |

Over the regression database (entries are the dump's `-- Name:` sections; round 19's run of
`pg_dump_roundtrip` on a full `pg_regress` run of this batch, 51,148 units matching):

| run | round 17 | round 19 |
|---|---|---|
| plain, custom, directory, directory `-j 4`, tar, `--inserts`, `--column-inserts` | 2,690 / 2,733 | 2,719 / 2,733 |
| `--schema-only` | 2,025 / 2,031 | 2,031 / 2,031 |
| `--data-only` | 658 / 695 | 681 / 695 |
| `--no-owner --no-privileges` | 2,675 / 2,718 | 2,704 / 2,718 |
| `-n public -N regress_*` | 2,218 / 2,257 | 2,246 / 2,257 |
| `-t tenk1 -t onek* -t public.road` | 20 / 21 | 20 / 21 |
| engine dump restored into the reference (pg_restore, `-j 4` from custom and directory, tar, psql) | 4 errors | 0 errors each; re-dumped: 2,704 / 2,733 |
| reference dump restored into the engine (the same five ways) | never finished | 0 errors each (15 before); re-dumped: 2,704 / 2,733, as the reference's own |
| `pg_dumpall --globals-only / --roles-only / --tablespaces-only` | | 7 / 7, 7 / 7, 5 / 5 |
| psql backslash commands (`psql_meta.sql`) | 122 / 146 | 140 / 146 (the 6 left: sizes, `\dnS`) |

Rounds 20-21: the engine's dump restored into the reference failed 4 statements again from
round 20 on (`mynums2`, a domain over a domain whose `typtypmod` showed its base's; the
regression is 590cb171's, hidden for a round by ddffa598's accidental revert); 0 again since
round 21 (agent 92, golden `a92_domain_typmods`), every way, with restores of the reference's
dump into the engine 0 errors too. Restoring the reference's dump into the engine took 35-62 s
against PostgreSQL's 3-4 s for the engine's dump.

What still differs in the data entries (14 of plain's 14) is the databases' state, not pg_dump:
values computed from `random()` / `now()` / `version()`, the oids in `dropped_objects`, and rows
in another physical order (`gtest29`, `rtest_emplog`: an UPDATE's new row versions placed
differently, so the rows a rule logs come in another order; `road`, `ihighway`, the spgist /
quad tables). The schema-only dump matches entry for entry.

A re-dump of a restored database is compared with the target side's own dump of its
regression database, so it also counts what a dump and restore don't keep on PostgreSQL itself:
29 entries on both sides alike (an inherited table's column order, row order, the
non-reproducible values). Before this round's last two fixes the engine's had 46: NULL-constant
defaults it stored where PostgreSQL drops them (`DEFAULT NULL::integer` after a restore), and
functions over `character`, whose parameter came back as `character(1)`.

## Bugs fixed (round 17, agent 37)

pg_dump couldn't dump any database at first; what it needed, and what the comparisons found:

- `"char"` → `text` is an implicit cast (pg_cast), as are `regprocedure` / `regoperator` /
  `regconfig` / `regdictionary` → int4 / int8 assignment casts: `relkind = ANY (array['r'])`
  and every pg_dump catalog query comparing a `"char"` column with a literal failed.
  `types::cast` now has a test comparing `cast_context` with every built-in pg_cast row.
- `acldefault()` takes a `"char"`.
- A `name` is a fixed-length array of `"char"` (raw_array_subscript_handler): `typname[0]`,
  `n[1] := 'x'`.
- `LOCK TABLE` on system catalogs (pg_dump locks them all for `-n pg_catalog`), `COPY` from a
  system catalog.
- `pg_export_snapshot()` and `SET TRANSACTION SNAPSHOT`: synchronized snapshots for `pg_dump
  -j` (exported per transaction, gone when it ends, PostgreSQL's identifiers and checks).
- **Concurrent DDL**: two sessions' DDL committing one after the other failed the second
  with "could not serialize access due to concurrent schema change" (the catalog is one
  versioned value), so `pg_restore -j` failed; worse, a DDL that started before another
  commit and saved after it recorded the newer catalog as its base and silently dropped the
  other transaction's changes. Commits now merge their catalog onto the current one
  (`txn/catmerge.rs`, a three-way merge of the serialized catalogs; one object changed by
  both is still a serialization failure, a name taken by both a unique violation), from the
  catalog the command actually started from.
- **Stored expressions name their objects for good**: defaults, generation expressions,
  CHECKs, index expressions and predicates, partition keys, statistics expressions,
  policies, trigger WHEN conditions and domain CHECKs are qualified when stored (as views,
  rules and SQL-standard bodies already were), as Postgres's stored trees name objects by
  oid. Before, a default `nextval('s')` made under one search_path failed (or used another
  object) under another, and pg_dump (search_path '') printed unqualified names. User
  operators are qualified as `OPERATOR(schema.op)`, base types and range constructors too.
  CHECK constraints merged with inherited copies compare qualification-insensitively.
- A generation expression's `tableoid` is the column, not the table's oid (it printed a
  constant).
- `pg_get_viewdef` under `restrict_nonsystem_relation_kind = view` (pg_dump sets it): a view
  over a view fell back to its raw text. Views over system catalogs print like any other.
- `SUPPORT`, a C function's library and link symbol (probin / prosrc), `WINDOW` (prokind
  'w'), `TRANSFORM FOR TYPE` (protrftypes, with PostgreSQL's check) are kept and printed.
- `ALTER VIEW ... ALTER COLUMN ... SET DEFAULT` is a pg_attrdef row (and atthasdef).
- Domains keep `COLLATE` (typcollation) and store `DEFAULT` cooked (typdefault /
  typdefaultbin); a range's `SUBTYPE_OPCLASS` is rngsubopc (and a dependency), a range over a
  domain has the domain as rngsubtype.
- Arrays keep their element's type modifier in atttypmod / typtypmod (`character(5)[]`).
- Partition bounds quote constants as get_const_expr does (only non-negative int4 and
  float-looking numerics go bare).
- `ALTER COLUMN SET COMPRESSION` and `SET ( options )` / `RESET` don't recurse to children.
- System catalogs: column privileges (system columns too), and `REVOKE` starts from initdb's
  ACL (`REVOKE SELECT ON pg_proc FROM public` did nothing).
- A composite type's column comment is pg_class's (pg_description.classoid).
- A whole-row CHECK depends on its table alone (pg_dump dumped it apart from the table).
- A serial's sequence kept when the column becomes an identity is only owned by it
  (deptype 'a'); pg_get_serial_sequence returns it, as PostgreSQL does.
- `CREATE PUBLICATION ... WITH (publish = '')` publishes nothing.
- `roll-engine` shuts down cleanly on SIGTERM / SIGINT, and closing a database (idle or at
  shutdown) keeps its sequences' positions: they came back 32 values ahead after every
  restart, as PostgreSQL's only do after a crash.

Second batch, from the round trips and psql:

- `ALTER DATABASE ... SET / RESET` was a no-op: it is pg_db_role_setting's (setrole 0) entry
  now, which sessions there start with (`\drds`, `\dconfig`, pg_dumpall).
- `pg_tablespace_size`, and `pg_database_size(oid)` sizes that database (it sized the
  current one); unknown oids error as in PostgreSQL. `format_type(0)` is `-` (`\do`).
- `pg_get_ruledef`: a condition with a subquery and an action writing a view printed
  nothing (pg_dump dropped those rules).
- A range type's constructors: pg_get_function_arguments / _result (`\df`), and qualified
  names in deparsed expressions.
- A domain CHECK's `VALUE[1]` prints `(VALUE)[1]`; a domain-typed column without COLLATE has
  the domain's collation.
- The regression harness and this one stop the engine with SIGTERM.

Goldens: `tests/sql/a37_pgdump_catalog.sql`, `a37_pgdump_catalog2.sql`,
`a37_stored_expr_path.sql`, `a37_psql_catalog.sql`; wire tests `exported_snapshots`,
`concurrent_ddl`, `sequences_survive_a_clean_close`, `database_settings`.

## Bugs fixed (round 19, agent 54)

- **Databases' own options**: `CREATE / ALTER DATABASE ... IS_TEMPLATE / ALLOW_CONNECTIONS /
  CONNECTION LIMIT` and a superuser's `UPDATE pg_database SET datistemplate / datallowconn /
  datconnlimit` are kept with the cluster's databases (`engine.rs` `DbEntry`,
  `session/ddl/database.rs`) and shown in pg_database (template0 and template1 are templates);
  a rollback puts them back. Enforced as PostgreSQL does, messages and codes recorded from it:
  refused connections, "too many connections for database", "permission denied to copy
  database" (a non-template only by its owner), "cannot drop a template database" (by
  datistemplate, 42809), "cannot disallow connections for current database", and invalid
  databases (datconnlimit -2): "cannot connect to invalid database", "cannot use invalid
  database ... as template", FATAL "cannot alter invalid database", skipped by pg_dumpall.
  initdb's templates have its ACL (`=c/postgres`, `postgres=CTc/postgres`). Wire test
  `database_options`. (4 of 002's 5 failures.)
- `ALTER FUNCTION / AGGREGATE <built-in> OWNER TO` (pg_proc.proowner of built-ins,
  `Catalog::builtin_proc_owners`, catalog record tag 49; its ACL's owner entries move).
- **PostgreSQL's dump restored into the engine had 15 errors**, all fixed: `ALTER FUNCTION`
  naming a shell type in its signature and `ALTER TYPE <shell> OWNER` / GRANT on one
  (LookupTypeName finds shells); an identity column's sequence options `LOGGED` / `UNLOGGED`
  (pg_dump's, for a sequence whose persistence isn't its table's); a view over a SQL function
  whose body names a table off the restore's empty search_path (a stored query doesn't read
  the body: only planning, inlining, does); COPY into a table with no columns; COPY of
  regclass / regtype / ... values (their input had no catalog); the default operator class of
  a binary-coercible type (cidr's is inet's: GetDefaultOpClass), which broke `CREATE INDEX ...
  USING brin (cidrcol)` and every index of cidr without an opclass.
- **Concurrent DDL silently corrupted indexes**: two transactions making indexes of one table
  at once (`pg_restore -j` does) gave both the same storage id (`next_index_id`, a per-table
  counter both took the same value of), so each index read the other's entries (`WHERE y = 7`
  returned 2 rows); when the counters differed, the commit failed with a serialization error
  instead (pg_restore -j's one error). A new index's storage id is its oid
  (`Table::new_index_id`). Wire test `concurrent_ddl`, which fails without it.
- **Concurrent DDL's merge was O(catalog)** (`txn/catmerge.rs`): it serialized the three
  catalogs whole to JSON for every commit that raced another, under the commit lock;
  `pg_restore -j 4` of the regression database took 110-137 s against 6 s serially, and of 3,000
  tables more than 30 minutes. It now compares the catalogs record by record as a commit stores
  them (`persist::record_changes`), takes the current catalog with this transaction's records,
  merging only records both changed: the regression database 5 s with `-j 4`, 3,000 tables
  254 s (see the benchmark below).
- A range constructor over a domain shows the casts to the domain (`d1range((a)::d1, ((a +
  5))::d1)`), not added twice when re-read.
- A NULL-constant default (`DEFAULT NULL`, `NULL::integer`) isn't stored, as in PostgreSQL;
  `SET DEFAULT NULL` removes the default.
- A function's parameter and result types drop their modifiers (`character` was
  `character(1)`, and `f('abc')` failed "value too long for type character(1)").
- psql: range constructors are visible to `\df` (pg_function_is_visible), unlogged sequences
  have their init fork in their size (`\ds+`).
- Harness: restores in every format and with `pg_restore -j 4` both ways, their times; psql's
  output normalized for what PostgreSQL varies too (`\watch`'s clock, large objects' ids, the
  oids ECHO_HIDDEN's queries name); SCRAM secrets' salts; the engine started with the
  reference's password.

Goldens: `tests/sql/a54_restore_fixes.sql`; wire tests `database_options`, `concurrent_ddl`.

## pg_dump and pg_restore at scale (round 19)

`scale_setup.sql` (`psql -v big=20000000 -f`): 10 schemas × 300 tables (bigserial key, an index, a
CHECK, a comment, 20 rows), 1,000 views, one table of 20,000,000 rows (~3 GB in PostgreSQL)
with two indexes; 19,420 pg_class rows. Release engine and the pinned reference on the same 4
cores (8-11), the client programs unpinned; seconds:

| step | PostgreSQL 3,000 tables | engine | PostgreSQL + 20 M rows | engine |
|---|---|---|---|---|
| build it (psql) | 19.6 | 70.5 | 105 | 454 |
| database size | 235 MB | 12 MB | 3,015 MB | 868 MB |
| `pg_dump -Fc` | 3.6 | 5.9 | 59.6 | 87.9 |
| `pg_dump --schema-only` | 2.1 | 4.4 | 1.9 | 3.5 |
| `pg_dump -Fd -j 4` (synchronized snapshots) | 3.0 | 12.1 | 54.8 | 85.5 |
| `pg_restore` | 24.5 | 75.3 | 66.9 | 453 |
| `pg_restore -j 4` | 18.8 | 253.9 | 52.7 | 600 |

Both restores give the same rows (count and sum checked) without errors. pg_dump's catalog
queries take about twice PostgreSQL's time (4.4 s for the schema of 3,000 tables). `pg_dump -j`
gains nothing on the engine (each worker's session regenerates the catalog's rows for its own
snapshot), and parallel restore is still slower than a serial one: every DDL commit that
raced another still merges, reindexes the whole merged catalog (`Catalog::reindex`) and checks
its names under the commit lock, O(catalog) per commit. Bulk loading (COPY of 20 M rows,
index builds) is the rest of the restore's time.

## What's left

- **Sizes** (decided: format and magnitude, not bytes). Relations' sizes (`pg_relation_size`,
  `pg_table_size`, `\dt+`, `\di+`, `\dm+`, `\ds+`) are PostgreSQL's page model
  (`catalog/heapsim.rs`, `btsim.rs`, ...), equal where the state is: PostgreSQL's differ where
  autovacuum made a table's FSM and visibility map (+32 kB, the engine has no autovacuum) and
  where an index grew by inserts the model lays out otherwise. Database and tablespace sizes
  (`pg_database_size`, `pg_tablespace_size`, `\l+`, `\db+`) are the engine's storage on disk,
  compressed: the regression database 11 MB (PostgreSQL 91 MB), 20 M rows 868 MB (3 GB);
  `pg_global` is 0 (the shared catalogs are in the cluster's registry file). pg_size_pretty
  formats them as PostgreSQL does. (A database's size as the sum of its modelled relations
  would cost a model of every relation per call.)
- psql `\dnS`: the engine keeps `pg_temp_N` schemas, but numbers them by backend pid
  (`pg_temp_3595`), PostgreSQL by ProcNumber (small, reused slots that its auxiliary
  processes share): not reproducible without modelling them.
- Restore speed: serial restores are 3-7x PostgreSQL's (bulk loads, per-statement DDL);
  `pg_restore -j` and `pg_dump -j` are slower than serial ones (above). Agent 49's catalog
  work is the place: an incremental `reindex` and name check for a merged catalog, and
  catalog rows shared across snapshots.
- Row order after UPDATEs differs in a few tables (data entries above).
- `003_pg_dump_with_server.pl` (a test extension in the server) and `010_dump_connstr.pl`
  (clusters made with other bootstrap superusers) don't run.
