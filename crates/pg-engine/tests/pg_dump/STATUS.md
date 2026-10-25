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

| measure | before (2d82227) | now |
|---|---|---|
| 002_pg_dump.pl TAP checks | 32 / 14,609 (pg_dump could not start) | 14,604 / 14,609 |
| 001_basic.pl, 005_pg_dump_filterfile.pl | not run | 96 / 96, 108 / 108 |
| 004_pg_dump_parallel.pl (parallel dump and restore) | 2 / 4 | 4 / 4 |
| regression database dumps (pg_dump runs at all) | fails (`acldefault`, `name` subscripts, `"char" = text`, `pg_export_snapshot`) | every format and option dumps |

Over the regression database (round 17's run; entries are the dump's `-- Name:` sections):

| run | entries matching |
|---|---|
| plain, custom, directory, directory `-j 4`, tar, `--inserts`, `--column-inserts` | 2,690 / 2,733 |
| `--schema-only` | 2,025 / 2,031 (2,031 / 2,031 with this batch's fixes, measured per entry) |
| `--data-only` | 658 / 695 |
| `--no-owner --no-privileges` | 2,675 / 2,718 |
| `-n public -N regress_*` | 2,218 / 2,257 |
| `-t tenk1 -t onek* -t public.road` | 20 / 21 |
| engine dump restored into the reference (pg_restore, psql) | 4 errors, 2,675 / 2,733 |
| psql backslash commands (`psql_meta.sql`) | 122 / 146 (before this batch's psql fixes) |

The first round-trip run (before the fixes after it) had 2,647 / 2,738 plain entries and
1,990 / 2,033 schema entries matching. What still differs in the data and sequence entries is
the databases' state, not pg_dump: values computed from `random()` / `now()`, rows in another
physical order (UPDATEs' new versions placed differently), TOAST relation names (oids), the
large objects' oids, and the regression engine's sequences, which came back 32 values ahead
after the regression harness killed the engine (it now stops it with SIGTERM).

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

## What's left

- **Databases' own options**: `ALTER DATABASE ... CONNECTION LIMIT / ALLOW_CONNECTIONS /
  IS_TEMPLATE` and `UPDATE pg_database SET datconnlimit` are not kept (pg_database shows -1,
  true, false), so the invalid-database TAP checks (datconnlimit = -2: "cannot connect to
  invalid database", pg_dumpall skipping it) fail: 4 of 002's 5 failures. Needs the cluster
  registry's database entries to keep them (`engine.rs` `DbEntry`).
- **Locale provider**: every engine database reports the builtin provider with locale
  C.UTF-8 (and behaves so: `upper('é')` is 'É'), where the reference cluster (initdb
  `--locale=C`) and `CREATE DATABASE ... LOCALE 'C'` give libc C (`upper('é')` is 'é').
  pg_dumpall writes `LOCALE_PROVIDER = builtin ... BUILTIN_LOCALE = 'C.UTF-8'`; the 5th TAP
  failure. A decision about the engine's default collation, not taken here.
- `ALTER FUNCTION <built-in> OWNER TO` (002's setup) is refused.
- A range constructor over a domain subtype prints without the casts to the domain
  (`gtestdomain1range(a, a + 5)` for PostgreSQL's `...((a)::gtestdomain1, ((a + 5))::...)`).
- psql: sizes (`\dt+`, `\di+`, `\l+`, `\db+`) are the engine's storage's; `\dnS` lacks the
  `pg_temp_N` / `pg_toast_temp_N` schemas PostgreSQL keeps after sessions end; `\l` shows the
  locale provider above.
- Restoring the regression database into the engine is slow (a debug engine took over an
  hour for psql's 450,000-line restore): each DDL statement copies and stores the catalog,
  which grows with the database (agent 36's per-object catalog storage is the place).
- `003_pg_dump_with_server.pl` (a test extension in the server) and `010_dump_connstr.pl`
  (clusters made with other bootstrap superusers) don't run.
