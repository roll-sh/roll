# Requests from nosql-server to nosql-engine

Things the server needs from the engine. Each has the server-side workaround in use today; remove
the workaround when the request is done. Newest at the bottom.

## 1. `Send` futures — DONE (the `exec.rs` workaround is removed)

rustc can't prove these futures `Send` ("implementation of `Send` is not general enough",
rust-lang/rust#110338 — closures returning async blocks that borrow their argument):

- `Session::query(..)` and `Session::execute(..)` — the note names `&ModifyPlan`,
  `&QueryParams` and `&str` (the modification path in `statements/`).
- `Database::open_with(..)` — via `nosql_storage::db::Database::table`, whose
  `|s: &SstRef| async { .. }` closure has the same problem (storage crate).

Check: `fn is_send<T: Send>(_: T) {}  is_send(async move { s.query("x", QueryParams::default()).await; });`
and the same for `execute` and `Database::open_with(cfg, store, hooks)`. Usual fixes: make the
closure take owned values (`move |s: SstRef| async move { .. }` over cloned items), or replace
`.map(|x| async { .. })` + `buffered` with an explicit loop / `FuturesOrdered` of boxed
`BoxFuture<'static, _>`; avoid holding `&T` captured by an `async` block passed to a generic
combinator.

Workaround: `crates/nosql-server/src/exec.rs` runs every engine call on single-threaded
runtimes with a `LocalSet` (one hop per request).

## 2. Concurrent requests on one connection — DONE (measured cheap; `Database::session(state)`)

`Session` methods take `&mut self`, so one session can't run two requests at once. The server
creates a fresh `Session` per request (`db.connect(client)`), copies the connection's
`SessionState` into it, and copies `keyspace` back after a `SetKeyspace` result. Please keep
`Session` cheap to create and `SessionState` the complete per-connection state (anything a
later request must see — keyspace, role, …), or offer `&self` methods.

Done: measured at ~0.5 µs per session including the state copy with a driver's STARTUP map (debug
build; a point query is ~55 µs: `tests/live.rs` `per_request_sessions_are_cheap`), so `&self`
methods aren't needed. `Database::session(state)` creates one from the connection's state in one
step (the server no longer clones the client info twice).

## 3. Request tracing — DONE (`Session::set_tracing`, `crate::trace`; the server uses it, the workaround is removed)

The server has no way to pass the TRACING flag. Wanted: `QueryParams.tracing: bool` (or a
`Session` setter) so the engine records `system_traces.sessions/events` and returns
`Response.tracing_id`. Workaround: when tracing is requested and the engine returns no id, the
server sets a fresh timeuuid (so the frame has the right shape; the trace tables stay empty).

Done, as a setter (no `QueryParams` field, so the server's literal keeps compiling). What the
server should do:

- For a request frame with the TRACING flag, call `session.set_tracing(true)` on the per-request
  `Session` before `query` / `prepare` / `execute` / `batch`. The engine writes the session row
  (with `duration`) and the events before it returns, so `get_query_trace` finds them at once.
- Put `Response::tracing_id` on the RESULT frame (already done) and the new
  `Prepared::tracing_id` on a traced PREPARE's RESULT; drop the fresh-timeuuid workaround (an
  error frame carries no id: the engine traces it but returns none).
- REST (ScyllaDB `api`): `POST /storage_service/trace_probability?probability=p` →
  `db.set_trace_probability(p)` (errors outside 0..=1), `GET` → `db.trace_probability()`;
  `POST /storage_service/slow_query?enable=&ttl=&threshold=&fast=` →
  `db.set_slow_query_log(nosql_engine::trace::SlowQueryLog { .. })`, `GET` →
  `db.slow_query_log()` (`{"enable": false, "ttl": 86400, "threshold": 500000, "fast": false}`).

## 4. LWT marker on results — DONE (`Response::conditional`; `Prepared::is_lwt` for the PREPARED mark)

`Rows` should say whether it comes from a conditional statement (Cassandra never sends
`METADATA_CHANGED` for LWT results, CASSANDRA-13992). Workaround: a first column named
`[applied]` counts as conditional.

Done: `Response.conditional` is set on every conditional statement's or batch's result (on the
`Response` rather than `Rows`, so `Rows { .. }` literals elsewhere keep compiling); pass it to
`result_body` instead of the `[applied]` check (a `SELECT v AS "[applied]"` is not
conditional). `Prepared.is_lwt` is set for conditional statements and batches, and ScyllaDB's
PREPARED result metadata is the `[applied]` row the statement returns.

## 5. Dialect texts the server needs — DONE (`AuthStep::Success` is `Some(empty)` for ScyllaDB, `None` for Cassandra: `auth::success`)

- `messages::unprepared(d, id)` ignores the dialect: ScyllaDB says
  `No prepared statement with ID deadbeef found.` (captured; `docs/nosql/reference/supported.md`).
- A `messages::bad_credentials(d, user)` the server and the auth subsystem share. The server
  uses Cassandra's `Provided username {user} and/or password are incorrect` and, for
  ScyllaDB, `Username and/or password are incorrect` (verified against
  `nosql-ref-*-auth`, also for every malformed PLAIN token except an empty/null one:
  `Authentication ID must not be null`) when a SASL user names no database.
- The server parses SASL PLAIN itself and answers malformed and null tokens with each
  reference's text (`conn::sasl_error`), so the engine only ever sees well-formed tokens. A
  successful AUTH_SUCCESS token is empty bytes in ScyllaDB and null in Cassandra; the engine's
  `AuthStep::Success` should return `Some(Bytes::new())` for ScyllaDB.

## 6. `Session::authenticate` checks nothing yet — DONE (`crate::auth`; the platform superuser is `DbConfig::superuser` and the engine checks it, `conn::platform_only`)

It accepts any credentials. The server checks platform credentials (`DbSpec::credentials`)
itself and passes everything else (with the `@db` suffix stripped from the user) to
`authenticate`, so `PasswordAuthenticator` databases are only as strict as the engine.

Done. What the server should do:

- Pass the AUTH_RESPONSE token (with `@db` already stripped from the PLAIN user) to
  `Session::authenticate(&token)`. It returns `AuthStep::Success(None)` and sets
  `SessionState::user`, or `Err` with code `BadCredentials` and the reference's text
  (`Provided username x and/or password are incorrect` / ScyllaDB `Username and/or password are
  incorrect` (verified), `x is not permitted to log in`, the PLAIN format errors).
  With `AllowAllAuthenticator` it succeeds as the anonymous user (`user = None`).
- `nosql_engine::auth::requires_authentication(&cfg)` says whether to answer STARTUP with
  AUTHENTICATE; the class name is `org.apache.cassandra.auth.PasswordAuthenticator` in both
  dialects (verified on both auth-enabled references).
- Put the platform credential in `DbConfig::superuser` (`Superuser { name, password }`): the
  engine creates that role (`SUPERUSER = true AND LOGIN = true`) at open when it is missing, so
  `CassandraAuthorizer` treats a platform login as a superuser and `LIST ROLES` shows it. If the
  server keeps its own platform check and sets `SessionState::user` itself, the name must be that
  role. Rotate its password with `nosql_engine::auth::set_password(&db, name, password)` (takes
  effect at once). Without `superuser`, a Cassandra-dialect database gets the image's
  `cassandra`/`cassandra` role (only when no role exists); a ScyllaDB one gets none.
- Select the authorizer with `settings.authorizer = "CassandraAuthorizer"` (the `ReadOnly`
  wrapper in `node.rs` keeps working: it wraps `hooks.authorizer`).
- Request #5's shared text: `nosql_engine::auth::messages::bad_credentials(dialect, user)`.
- Alternator's SigV4 secret: `nosql_engine::auth::salted_hash(&db, role)`.

## 7. Events from the node — DONE (`Database::publish`; the node's own channel is gone)

The node sends `STATUS_CHANGE`/`TOPOLOGY_CHANGE` (database evicted or moved) on its own
channel and merges it with `Database::events()`. A `Database::publish(Event)` would let it use
one channel.

## 8. `Consistency::NodeLocal` — DONE

The protocol accepts `NODE_LOCAL` (11); the engine has no variant. The server maps it to
`LOCAL_ONE`.

## 9. STARTUP options for `system.clients` — DONE (`ClientInfo.options`, `Database::register_client`; the server registers every bound connection)

`ClientInfo` has no room for the STARTUP map (`client_options` in ScyllaDB's `system.clients`,
`TABLETS_ROUTING_V1` opt-in). Wanted: `ClientInfo.options: Vec<(String, String)>`. The server now builds `ClientInfo` from
`ClientInfo::default()` and sets fields, so adding one doesn't break it; it will fill `options`
with the STARTUP map once the field exists.

Done. `ClientInfo.options` holds the STARTUP map in the client's order. The clients tables list
*registered connections* (the server creates a `Session` per request, so sessions can't be the
list):

- After STARTUP, `let handle = db.register_client(client_info)`; keep it for the connection's
  life (dropping it removes the row).
- `handle.authenticated(user)` after AUTH_SUCCESS (`None` for anonymous), or `handle.ready()`
  when no authentication is needed; `handle.set_keyspace(..)` after a `SetKeyspace` result;
  `handle.request()` per request (`request_count`); `handle.set_tls(Some(Tls { protocol,
  cipher_suite }))` on TLS connections; `handle.set_shard(n)` for shard-aware ports.

## 10. Differences the compat harness found — DONE (`DROP TABLE nope.t` names the table; the `!todo` is gone)

Lines marked `!todo` in `tests/compat/*.cql` (run with `ROLL_NOSQL_REF_SCYLLA` /
`ROLL_NOSQL_REF_CASSANDRA` set); the harness fails once one matches, so remove the marker then.

- `errors.cql` `DROP TABLE nope.t` (Cassandra): reference `Table 'nope.t' doesn't exist`, roll
  `Keyspace 'nope' doesn't exist`.
