# nosql-control: requests to other crates

What the control plane needs from `nosql-engine` / `nosql-server` / `nosql-storage` and works
around today.

## nosql-engine

1. **Platform masking policy.** Done: `DbConfig.masking` (`(keyspace, table, column)` →
   `nosql_engine::config::MaskCall`) is applied at read time for every role, superusers
   included, in both dialects (`nosql_engine::masking`); `spec_for` passes the stored policy
   (branches inherit it with the rest of the row) and `PATCH masking` applies it live with
   `Database::set_masking`. A call that doesn't fit its column's type masks with `mask_default`.
2. **Live settings.** — DONE: `Database::set_settings` (thresholds, guardrails, authorizer, auth
   caches; `nosql_engine::live::RELOAD_SETTINGS` still reload) and `Node::set_read_only`; PATCH
   reloads only for those.
   `DbConfig` is fixed when a database opens, so a settings change
   (`settings`, `read_only`) reloads the database (drivers get `STATUS_CHANGE DOWN` and
   reconnect). Wanted: `Database::set_settings(Settings)` (thresholds, guardrails, auth caches)
   so most changes apply without a reload.
3. **Recent warnings per database.** — DONE: `Database::recent_warnings()` (last 100, every
   warning sent to a client) in `activity.warnings`; `slow_queries` reads ScyllaDB's slow log.
   `GET /databases/{id}/activity` returns `warnings: []`.
   Wanted: a bounded ring of recent client warnings (tombstone and batch-size thresholds,
   aggregation without a partition key) with time, statement and role, e.g.
   `Database::recent_warnings()`; and the slow query log filled in when tracing records it.

## nosql-server

4. **Idle timeout changes.** — DONE: `Node::set_idle_timeout`, applied by PATCH.
   `Loaded.spec` is fixed at load, so a new `idle_timeout` applies from
   the next load. Wanted: `Node::set_idle_timeout(id, Option<Duration>)` (as in
   `redis_server::node`).
5. **Connected clients.** — DONE: the server registers its connections; `activity.clients`
   lists them.
   `activity.clients` reads `system_views.clients` / `system.clients`
   (normalised to `address: "ip:port"`, `username`, `driver_name`, `keyspace`), but the server
   doesn't call `Database::register_client` for its connections yet, so the list is empty.

## nosql-storage

6. **First manifest version.** — DONE: storage writes a numbered first version when a database
   opens without a manifest; the control plane's `write_manifest(true)` is gone.
   A new database's first manifest is written without a numbered
   version, so a point-in-time branch or restore between creation and the first flush finds no
   state. The control plane writes `write_manifest(true)` right after creating a database;
   `Database::open` of a database without a manifest could do it instead.
