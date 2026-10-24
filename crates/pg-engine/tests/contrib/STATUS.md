# contrib extensions: status

The contrib modules' own regression tests (`contrib/<module>/sql` at `REL_18_6`) run through
the engine and a reference PostgreSQL 18.6 and compared unit by unit, as the main suite is
(`pg_contrib` in `tests/pg_regress.rs`; its doc comment has the options). `REPORT.md` is the
last run's per-file table.

## How extensions work here

- An extension is PostgreSQL's own control file and scripts (`src/ext/files/`, copied from
  REL_18_6 contrib), run by CREATE EXTENSION / ALTER EXTENSION UPDATE
  (`src/session/ddl/extension.rs`) the way `extension.c` runs them. Its objects, their OIDs,
  dependencies (pg_depend `e` for members, `n` for required extensions), update paths and
  pg_available_extension_versions all follow from that.
- `LANGUAGE C` functions are native Rust code found by library and symbol
  (`src/ext/native.rs`, one module per extension under `src/ext/`).
- Only the extensions with files in `src/ext/files/` are available. The older stand-ins
  (`session::ddl::EXTENSIONS`: CREATE EXTENSION accepted, nothing installed) remain for the
  ones not ported yet.

## Running

```sh
# a reference of the pinned image started with the modules' temp-config settings
docker run -d --name a39-ref -p 127.0.0.1:55939:5432 -e POSTGRES_PASSWORD=pw \
  -e POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" -v $D:$D <pinned image> \
  -c shared_preload_libraries=pg_stat_statements -c max_prepared_transactions=5
PGCONTRIB_ONLY=uuid-ossp,citext PGREGRESS_DIR=$D PGREGRESS_CONTAINER=a39-ref \
  PGREGRESS_ENGINE_CONTAINER=<psql container on the host network> PGREGRESS_PORT=55940 \
  cargo test -p pg-engine --test pg_regress pg_contrib -- --ignored --nocapture
```

## Scores

Baseline (before any extension was ported; CREATE EXTENSION was a stand-in): 2,752 / 7,733
units (35.6%). The reference matches `expected/` for every module except postgres_fdw and
dblink (they connect to a loopback server).

| module | units | matching | status |
|---|---|---|---|
| uuid-ossp | 23 | 23 | done |
| pgcrypto | 369 | 36 | in progress |
| citext | 544 | 32 | in progress |
| hstore | 309 | 21 | not started |
| pg_trgm | 261 | 140 | not started (stand-in) |
| btree_gin | 297 | 245 | not started (stand-in) |
| btree_gist | 616 | 485 | not started (stand-in) |
| pg_stat_statements | 1013 | 728 | not started (stand-in) |
| unaccent | 34 | 6 | not started |
| fuzzystrmatch | 56 | 6 | not started |
| ltree | 363 | 13 | not started |
| intarray | 182 | 29 | not started |
| tablefunc | 91 | 31 | not started |
| cube | 316 | 13 | not started |
| earthdistance | 170 | 13 | not started |
| isn | 51 | 1 | not started |
| seg | 196 | 16 | not started |
| lo | 38 | 15 | not started |
| amcheck | 204 | 130 | not started |
| pageinspect | 291 | 102 | not started |
| pg_buffercache | 38 | 15 | not started |
| postgres_fdw | 2053 | 613 | not assessed |
| dblink | 218 | 55 | not assessed |
