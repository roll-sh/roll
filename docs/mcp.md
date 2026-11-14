# roll MCP server

roll is one MCP server at `/v1/mcp` with tools for every product: workspace projects, Postgres,
Redis, storage, streams, NoSQL, queues, tasks, roll code and its agents, search, secrets, apps and
previews. Agents connect with OAuth or a scoped token, and can do exactly what the user allowed.

- Code: `crates/roll-mcp` (the server and its tools), mounted by `crates/rolld/src/main.rs`; roll
  code's tools come from `crates/code-control/src/mcp.rs` through `crates/rolld/src/access.rs`.
- Permissions: [permissions.md](permissions.md). The same rules decide the HTTP API, the CLI and MCP.
- Tests: `crates/roll-mcp/tests/mcp.rs` (tools through the gate, approvals, audit) and the
  `tools_map_onto_classified_routes` unit test (every tool is a classified route).

## How it works

Every tool is one request to roll's HTTP API, made on the caller's behalf with the caller's own
token, through the same gate as any other request. So:

- **A tool can't do more than its route.** Each tool names its route (`_meta["roll/route"]`), and a
  test fails when a tool's route has no row in the permission catalogue.
- **A tool's annotations come from its route's action.** `readOnlyHint` is true when the route only
  reads (`read`, or `reveal`), and `destructiveHint` is true for `delete` and `deploy` routes and for
  arbitrary statements (`postgres_execute`). `_meta["roll/actions"]` lists the actions.
- **`tools/list` shows only tools the connection can use.** A token without Postgres `delete` doesn't
  see `postgres_delete_project`. The list is a convenience: the gate decides every call anyway.
- **Every call is in the audit log**, with the tool name and its arguments redacted
  ([permissions.md](permissions.md#audit)). The gate writes the row for a grant's calls; the MCP
  server writes it for the operator's.

roll code had an MCP server of its own at `/v1/code/mcp`, and it stays for roll code owner tokens
(docs/code/mcp.md). `/v1/mcp` fronts all of its tools under product names (`search_code` is
`code_search`, `land` is `code_land`, `list_agent_machines` is `agents_list_machines`), through the
gate. Scoped tokens can't call `/v1/code/mcp`: that server calls roll code's routes without the
gate.

## Connect

Use the API's address: `https://api.roll.sh/v1/mcp` on roll cloud, or `http://127.0.0.1:7070/v1/mcp`
for a local daemon. The dashboard's **MCP & access** page (`/dashboard/access`) shows each client's
config with your address filled in.

### Toolsets

Every product's tools are about 259 (222 of roll's own, ~21k tokens of what a model reads, and
roll code's), before an agent does anything. Connect to `/v1/mcp?toolsets=postgres,code` (or send `X-MCP-Toolsets: postgres,code`) to
list only those products' tools, as GitHub's MCP server does; the access tools (approvals,
`access_check`) are always listed. On a four-product node, Redis alone is 12 tools, ~2.4k tokens
instead of ~16k. An unknown name is `400`, naming the toolsets there are. The dashboard's connect
panel picks them. A toolset only narrows the list: the token's rules decide every call as before,
and OAuth's resource is the server whatever the toolsets (checked with the TypeScript SDK).

Clients cap how many tools they take. VS Code refuses a chat request with more than 128 tools, its
own built-in ones included ("Cannot have more than 128 tools per request"); Windsurf takes 100;
Cursor is reported to drop tools past about 40. Claude Code defers a long list behind its own tool
search. So with every product on, VS Code, Windsurf and Cursor need toolsets: the connect panel
adds up a choice (`GET /v1/mcp/toolsets`: each product's tools, read-only tools and context cost,
to a caller) and warns past the picked client's limit.

The whole list stays the default. A smaller default would quietly drop tools from connections
that use them. A search-then-call pair of meta-tools (GitHub's dynamic toolsets) would hide each
tool's read-only and destructive hints, which clients use to decide what to run without asking.

### Read-only

`/v1/mcp?readonly=1` (or `X-MCP-Readonly: true`) is a read-only connection, as GitHub's server's
read-only mode is: only tools annotated `readOnlyHint` are listed and run, whatever the token
allows (a call to another is an error result), and prompts that need one aren't offered. A tool is
read-only when its route only reads, or reveals with a `GET`: a reveal that's a `POST` makes
something someone can use (a presigned PUT URL, a share link), so it isn't. It narrows, never
widens: each call is still decided by the token's rules. It's on when the query or the header asks;
anything but `1`, `true`, `0` or `false`, or the key spelled another way (`read_only`), is a 400,
so a typo never means "not read-only". The connect panel has a checkbox
for it, and it goes with toolsets (`?toolsets=postgres&readonly=1`).

### With OAuth (Claude Code, Claude.ai, Cursor, VS Code, …)

Add the server by its URL. The client finds roll's authorization server, registers itself, and
opens the browser on roll's consent screen. There you see the client, choose what it may do (a
preset, then any narrowing), and approve. The client gets a one-hour access token and a refresh
token.

```sh
# Claude Code
claude mcp add --transport http roll https://api.roll.sh/v1/mcp
```

```json
// Cursor (~/.cursor/mcp.json), VS Code (.vscode/mcp.json uses "servers"), Windsurf
{ "mcpServers": { "roll": { "url": "https://api.roll.sh/v1/mcp" } } }
```

Claude.ai: Settings → Connectors → Add custom connector → `https://api.roll.sh/v1/mcp`.

### With a scoped token

Create a token with the builder (`/dashboard/access/new`) or `roll token create`, then send it as a
bearer token. This works for any client that can set a header (and for CI, scripts and agents that
call the API directly).

```sh
claude mcp add --transport http roll https://api.roll.sh/v1/mcp --header "Authorization: Bearer rlt_…"
```

```json
{ "mcpServers": { "roll": { "type": "http", "url": "https://api.roll.sh/v1/mcp", "headers": { "Authorization": "Bearer rlt_…" } } } }
```

`X-Roll-Agent: <name>` (optional) is the name roll code reviews and comments as.

A local daemon without `ROLL_ADMIN_TOKEN` is open: `/v1/mcp` works without a token, as the
operator. A scoped token sent to it is still held to its rules.

## Approvals

A rule can say `ask` for an action. A tool call that needs it waits up to 55 seconds for the user to
approve it in the dashboard (or `roll mcp approve`). Approved, the call runs and returns its result.
Still waiting, the result is not an error:

```json
{ "status": "approval_pending", "approval": "apr_…", "url": "https://roll.sh/dashboard/access/approvals?id=apr_…" }
```

The agent tells the user, then calls the same tool again with the same arguments (`"approval":
"apr_…"` too, though the same call finds its approval without it). That waits again and runs the
call once it's approved. Only a connection a person can be asked about is offered the `approval`
argument: a token with an `ask` rule, or one that asks for anything else (the operator's and a
token that only allows never wait, and drop it from every schema: 92 → 73 KB of what a model reads
with every product on). An approval runs exactly the request it was asked for (same method, path
and body), once, and asking the same thing again waits on the same approval rather than making
another. `access_wait_approval` waits for a decision without running anything. A denied call
fails with the user's reason.

A client that opens URLs for elicitation (it declares `capabilities.elicitation.url` in
`initialize`, MCP 2025-11-25) gets the approval as one instead: the call answers the JSON-RPC error
`-32042` with `data.elicitations[0]` = `{mode: "url", elicitationId: "apr_…", url, message}` (and
`data.approval`) at once, without waiting. The client shows the person the approval page, and sends
the call again once they've decided; the gate finds its approval (and a denial answers with the
person's reason).

## Authorization

roll implements the MCP authorization spec (2025-06-18 and 2025-11-25) with OAuth 2.1:

| step | endpoint | spec |
| --- | --- | --- |
| a call without a token gets `401` with `WWW-Authenticate: Bearer resource_metadata="…"` | `/v1/mcp` | RFC 9728 §5.1 |
| protected resource metadata: the resource and its authorization server | `GET /.well-known/oauth-protected-resource[/v1/mcp]` | RFC 9728 |
| authorization server metadata | `GET /.well-known/oauth-authorization-server`, `/.well-known/openid-configuration` | RFC 8414 |
| dynamic client registration (public clients only), or a client ID metadata document | `POST /v1/access/oauth/register`; the client's own URL | RFC 7591; MCP 2025-11-25 |
| authorization: the dashboard's consent screen | `GET <dashboard>/oauth/authorize` | OAuth 2.1 §4.1 |
| tokens: `authorization_code` (PKCE `S256` required) and `refresh_token` | `POST /v1/access/oauth/token` | OAuth 2.1, RFC 7636, RFC 8707 |
| revocation | `POST /v1/access/oauth/revoke` | RFC 7009 |

- The issuer is the API's origin, and the resource is `<api>/v1/mcp`. The token endpoint checks
  `resource` against the one the code was issued for (RFC 8707), so a roll token is never accepted
  for another server's resource.
- Redirect URIs must be `https`, `http` on a loopback address (RFC 8252), or an app's own scheme
  (`cursor://…`). Never `javascript:`, `data:` or `file:`. The redirect must match a registered URI
  exactly.
- Registration is open, so it stores nothing: a registered client's `client_id` is the client
  itself (`rlc_<payload>.<mac>`: its name, URI and redirect URIs, signed with a key every node
  shares), kept in `oauth_clients` only once a person consents to it. Until then the id works for 90
  days (clients keep their registration); together the name, URI and redirect URIs are at most
  1,500 bytes, so a kept id stays under Postgres's index row limit. A flood of registrations has nothing to fill, as a metadata document's client never did. A
  network (/24, /48) still registers at most 20 a minute on a node (`429` with `Retry-After`).
- Codes are good for one minute, once. Access tokens (`rlo_`) last an hour. Refresh tokens (`rlf_`)
  rotate on every use, and a used one fails; a connection expires 30 days after its last refresh.
- `scope` (`read-only`, `agent`, `project`, `development`, `previews`, `full`) picks the preset the
  consent screen starts from. The user decides what's granted, and the consent screen is the only
  place a connection's rules come from: a client can't widen them.
- Each connection is a grant: it's listed with its client, last use and rules, and revoking it ends
  every token it has, at once.
- Clients either register (dynamic registration) or are known by a client ID metadata document
  (2025-11-25: the `client_id` is an https URL whose JSON names it). A document is fetched through
  roll's private-address guard (`roll-net`), without redirects, in 5 seconds, at most 64 KiB, and
  at most once an hour; it must name itself, list valid redirect URIs, and be a public client.

## Protocol

- Streamable HTTP, JSON-RPC 2.0, protocol versions `2025-11-25`, `2025-06-18`, `2025-03-26` and
  `2024-11-05`.
- **Stateless:** every node of a cluster answers every request, and nothing is lost on a restart.
  `initialize` answers an `Mcp-Session-Id` that holds no state: its prefix says whether the client
  declared URL elicitation (`roll-url-1.` or `roll-1.`, then a random part, so ids are unique), so
  later calls know how to ask for approvals.
  A request without one works as before. `GET` and `DELETE` answer 405.
- Responses are JSON, or server-sent events when `Accept` has `text/event-stream` without
  `application/json`. Keep-alive comments come every 15 seconds while a call waits, for example on
  an approval.
- **Progress:** a `tools/call` with `_meta.progressToken`, from a client that accepts
  `text/event-stream`, is answered as a stream with a `notifications/progress` every 10 seconds
  until the answer ("still running (it may be waiting for a person to approve it)"), so a client
  that times out on silence keeps waiting for a person (checked with the TypeScript SDK's
  `onprogress`). The call stays the request's: a client that goes away cancels it, as before.
- `serverInfo` carries roll's Horizon mark as an icon (2025-11-25 `icons`, an inline SVG: the
  dashboard's favicon), for clients that show one next to the server.
- Requests with an `Origin` header must come from the dashboard's or the API's origin (DNS
  rebinding).
- Results come back as `structuredContent` (the route's JSON; arrays as `{"items": […]}`) and a
  `text` block, cut at 256 KiB. Binary content comes back base64 (`content_b64`). An image (a preview's screenshot: PNG, JPEG or WebP, up to
  4 MiB) comes back as MCP image content the agent can look at.
- Characters a person can't see but a model reads are escaped in the `text` block: tag characters
  (ASCII smuggled as U+E0000–E007F), zero-width and bidi controls, and variation selectors. They
  appear as JSON escapes (`\u200b`), so the text still parses to the same value, and
  `structuredContent` stays exact. Results carry other people's text (review comments, a
  preview's notes and mail, task messages, objects), and instructions hidden that way stay
  visible to the model.
- A route refusing a call is a result with `isError: true`, and its text says why ("not allowed:
  this token can't delete postgres prj_…"). Unknown tools and bad arguments are JSON-RPC errors.

## Resources

Two resources an agent can read before it acts (`resources/list`, `resources/read`;
`crates/roll-mcp/src/resources.rs`). `initialize`'s instructions point at both.

| URI | What it holds |
| --- | --- |
| `roll://guide` | How to use roll over MCP: ids, approvals, and what to do when a call is refused or rate limited (`src/guide.md`). |
| `roll://access/me` | What this connection may do, in words: each rule's products, actions (and which ask first), resources, environments and paths; what approvals allow it for a while and until when; what happens to the rest; its expiry, rate limit and address limit. A second content block holds `GET /v1/access`'s JSON. |

`roll://access/me` is read from `GET /v1/access` through the gated router with the caller's token,
as a tool is, so it says what the gate decides and the read is in the audit log.

One template (`resources/templates/list`), listed when the connection may list workspace projects:
`roll://projects/{project}` is a project and every resource on its canvas (kind and id, names
quoted), read from `GET /v1/workspace/projects/{project}` the same way, for a client to attach
before acting on a project. Its id completes like a prompt's (`completion/complete` with
`ref/resource`); an id that isn't letters, digits, `_` and `-` is "resource not found", so a URI
never reaches another route.

## Prompts

roll's workflows as prompts a client offers as slash commands (`prompts/list`, `prompts/get`;
`crates/roll-mcp/src/prompts.rs`), as GitHub's and Sentry's MCP servers ship theirs. A prompt is
only text that walks the agent through roll's tools: it calls nothing, each tool it names is still
decided by the gate, and a connection is offered only the prompts whose tools it has. Arguments are
ids (letters, digits, `_`, `.`, `-`, at most 64), quoted in the message: anyone can name a project,
and a prompt's text is the person's own message to the agent.

A prompt's `project`, `service` and `env` complete as the person types (`completion/complete`): the
values come from the list tool the connection would call (`workspace_list_projects`; a Postgres
project's from `postgres_list_projects`;
`apps_list_services`, `apps_list_environments` for the `service` already given), through the gate
with the caller's token, so a suggestion is only what it may read, and each is an audited read.
Only ids are offered (what tools take). A completion never asks a person: a list that would ask
is refused at once (`would_ask`), so typing opens no approval. A prompt the connection isn't offered
is `-32602`.

| Prompt | Arguments | What it does | Tools |
| --- | --- | --- | --- |
| `what_can_i_do` | | What this connection may do, in words, and which rule would allow what's missing. | `access_whoami` |
| `check_diff_for_secrets` | `project` | Before a commit: whether any added value is one of the project's stored secrets (by key and environment, never the value). | `secrets_scan` |
| `fix_secrets_health` | `project` | What needs fixing in a project's secrets, most urgent first, asking before changing anything. | `secrets_health` |
| `try_migration_on_branch` | `project` | A schema change tried on a branch first (create, migrate, compare), then on main only when it worked and you say so. | `postgres_get_project`, `postgres_create_branch`, `postgres_describe_schema`, `postgres_execute`, `postgres_delete_branch` |
| `compare_branch_schemas` | `project`, `branch` | What a branch changed in the schema against main (Neon's schema compare), and the advisors' findings there; changes nothing. | `postgres_get_project`, `postgres_describe_schema`, `postgres_advisors` |
| `debug_failing_service` | `service`, `env` | Why an environment is failing (deploys, deploy logs, failing requests, metrics), then a fix or a rollback it asks about first. | `apps_list_deploys`, `apps_deploy_logs`, `apps_logs`, `apps_metrics` |

## Tools

Arguments are checked against each tool's JSON Schema (closed: unknown arguments are refused).
Every tool also takes `approval` (see [Approvals](#approvals)). Creating tools take
`workspace_project`, the workspace project the new resource goes in; a token limited to projects
needs it. Tools for products that are off aren't listed. Tools for search, secrets, apps and
previews are listed once those products are on.

| tool | what it does | route |
| --- | --- | --- |
| `access_whoami` | The connection's identity: the operator, or the token or OAuth connection and the rules it has. | `GET /v1/access` |
| `access_wait_approval` | Wait (up to 55 s) for a person to decide an approval this connection asked for; then call the original tool again with approval. | `GET /v1/access/approvals/{approval_id}` |
| `access_check` | Whether this connection may make a request (allow, ask or deny), and as which action on what, without making it. | `POST /v1/access/check` |
| `access_list_approvals` | Approvals, newest first: pending ones wait for the operator. | `GET /v1/access/approvals` |
| `access_audit_log` | Every request made with a token and every MCP tool call, newest first. | `GET /v1/access/audit` |
| `workspace_list_projects` | Workspace projects, each with the resources on its canvas (kind, ref, position). | `GET /v1/workspace/projects` |
| `workspace_get_project` | A workspace project and its resources. | `GET /v1/workspace/projects/{id}` |
| `workspace_create_project` | A new workspace project to group resources. | `POST /v1/workspace/projects` |
| `workspace_update_project` | Rename a project or change its description. | `PATCH /v1/workspace/projects/{id}` |
| `workspace_delete_project` | Delete a workspace project. Its resources stay; only the grouping goes. | `DELETE /v1/workspace/projects/{id}` |
| `workspace_place_resource` | Put a resource on a project's canvas (or move it), at x, y. | `PUT /v1/workspace/projects/{id}/resources/{kind}/{ref}` |
| `workspace_remove_resource` | Take a resource off a project's canvas (the resource itself stays). | `DELETE /v1/workspace/projects/{id}/resources/{kind}/{ref}` |
| `postgres_list_projects` | Postgres projects with their branches and endpoints. | `GET /v1/projects` |
| `postgres_get_project` | A Postgres project with its branches and endpoints (connection strings need reveal). | `GET /v1/projects/{project}` |
| `postgres_create_project` | A new serverless Postgres database (a project with a main branch and endpoint). | `POST /v1/projects` |
| `postgres_delete_project` | Delete a project with all its branches and data. Irreversible. | `DELETE /v1/projects/{project}` |
| `postgres_create_branch` | A copy-on-write branch of a project, from its parent now or at a point in time. | `POST /v1/projects/{project}/branches` |
| `postgres_delete_branch` | Delete a branch and its compute. | `DELETE /v1/projects/{project}/branches/{branch}` |
| `postgres_branch_history` | The range a branch can be restored to or branched from. | `GET /v1/projects/{project}/branches/{branch}/history` |
| `postgres_restore_branch` | Rewind a branch in place to a past point (the current state is kept as a branch). | `POST /v1/projects/{project}/branches/{branch}/restore` |
| `postgres_reset_branch` | A branch back to its parent's current state, in place (Neon's reset from parent). | `POST /v1/projects/{id}/branches/{branch}/reset` |
| `postgres_get_endpoint` | An endpoint's compute state and limits. | `GET /v1/endpoints/{id}` |
| `postgres_update_endpoint` | An endpoint's autoscaling limits and suspend timeout. | `PATCH /v1/endpoints/{id}` |
| `postgres_query` | Run read-only SQL on an endpoint: it runs in a read-only transaction, so writes fail. Returns columns and rows. | `POST /v1/endpoints/{endpoint}/query` |
| `postgres_execute` | Run SQL that may change data or schema (INSERT, UPDATE, DDL) on an endpoint. | `POST /v1/endpoints/{endpoint}/query` |
| `postgres_describe_schema` | Every table's columns and types on an endpoint (read-only). | `POST /v1/endpoints/{endpoint}/query` |
| `postgres_explain` | A statement's plan as JSON, without running it (Neon's explain). | `POST /v1/endpoints/{id}/query` (read-only, `EXPLAIN (FORMAT JSON)`) |
| `postgres_slow_queries` | The 20 slowest statements by mean time (Neon's slow queries; `pg_stat_statements`). | `POST /v1/endpoints/{id}/query` (read-only) |
| `postgres_advisors` | Unused indexes, unindexed foreign keys, tables without a primary key, sequential scans (Supabase's advisors). | `POST /v1/endpoints/{id}/query` (read-only) |
| `postgres_list_extensions` | Extensions installed and available. | `POST /v1/endpoints/{id}/query` (read-only) |
| `postgres_typescript_types` | TypeScript interfaces for every table (Supabase's generated types). | `POST /v1/endpoints/{id}/query` (read-only) |
| `postgres_start_endpoint` | Wake an endpoint's compute. | `POST /v1/endpoints/{endpoint}/start` |
| `postgres_suspend_endpoint` | Suspend an endpoint's compute. | `POST /v1/endpoints/{endpoint}/suspend` |
| `postgres_usage` | Compute and storage use of a project, with cost estimates. | `GET /v1/projects/{project}/usage` |
| `redis_list_databases` | Serverless Redis databases. | `GET /v1/redis/databases` |
| `redis_get_database` | A Redis database's settings and connection (credentials need reveal). | `GET /v1/redis/databases/{database}` |
| `redis_create_database` | A new serverless Redis database. | `POST /v1/redis/databases` |
| `redis_delete_database` | Delete a database and its data. Irreversible. | `DELETE /v1/redis/databases/{database}` |
| `redis_create_branch` | A fork of a database's data, now or at a point in time. | `POST /v1/redis/databases/{database}/branches` |
| `redis_command` | Run one command on a database, e.g. ["GET", "key"]. Read-only commands are reads; FLUSHDB and FLUSHALL delete. | `POST /v1/redis/databases/{database}/command` |
| `redis_restore` | Rewind a database in place to a point in time. | `POST /v1/redis/databases/{id}/restore` |
| `redis_usage` | A database's usage report. | `GET /v1/redis/databases/{database}/usage` |
| `storage_list_buckets` | S3-compatible buckets. | `GET /v1/storage/buckets` |
| `storage_get_bucket` | A bucket's settings. | `GET /v1/storage/buckets/{bucket}` |
| `storage_create_bucket` | A new bucket. | `POST /v1/storage/buckets` |
| `storage_delete_bucket` | Delete an empty bucket (force deletes every object too, irreversibly). | `DELETE /v1/storage/buckets/{bucket}` |
| `storage_list_objects` | Objects in a bucket, by prefix; with delimiter "/" it lists one level like a folder. | `GET /v1/storage/buckets/{bucket}/objects` |
| `storage_head_object` | An object's size, type, ETag and metadata. | `GET /v1/storage/buckets/{bucket}/object` |
| `storage_read_object` | An object's content: text as is, anything else as base64. | `GET /v1/storage/buckets/{bucket}/download` |
| `storage_write_object` | Upload an object (replacing the key): content as text, or content_b64. | `PUT /v1/storage/buckets/{bucket}/upload` |
| `storage_delete_object` | Delete an object (to the trash; with version_id, that version for good). | `DELETE /v1/storage/buckets/{bucket}/object` |
| `storage_presign` | A URL anyone can use to get (or put) an object until it expires. | `POST /v1/storage/buckets/{bucket}/presign` |
| `storage_branch_bucket` | A copy-on-write branch of a bucket. | `POST /v1/storage/buckets/{name}/branches` |
| `storage_list_trash` | A bucket's deleted objects. | `GET /v1/storage/buckets/{name}/trash` |
| `storage_restore_object` | An object back from the trash. | `POST /v1/storage/buckets/{name}/trash/restore` |
| `storage_usage` | A bucket's usage. | `GET /v1/storage/buckets/{bucket}/usage` |
| `streams_list` | Kafka-compatible streams. | `GET /v1/streams` |
| `streams_get` | A stream's bootstrap address, topics and throughput. | `GET /v1/streams/{stream}` |
| `streams_create` | A new stream (its first API key needs reveal to see). | `POST /v1/streams` |
| `streams_delete` | Delete a stream with its topics and data. Irreversible. | `DELETE /v1/streams/{stream}` |
| `streams_list_topics` | A stream's topics. | `GET /v1/streams/{stream}/topics` |
| `streams_create_topic` | A new topic. | `POST /v1/streams/{stream}/topics` |
| `streams_delete_topic` | Delete a topic and its records. | `DELETE /v1/streams/{stream}/topics/{topic}` |
| `streams_produce` | Append records to a topic: [{key?, value, headers?, partition?}]. | `POST /v1/streams/{stream}/topics/{topic}/records` |
| `streams_read` | Records from a topic, from an offset or a time. | `GET /v1/streams/{stream}/topics/{topic}/records` |
| `streams_list_groups` | Consumer groups with their lag. | `GET /v1/streams/{stream}/groups` |
| `streams_reset_group` | Move a group's offsets (earliest, latest, an offset or timestamp:<ms>). | `POST /v1/streams/{stream}/groups/{group}/reset` |
| `streams_get_topic` | A topic's partitions, offsets and settings. | `GET /v1/streams/{s}/topics/{t}` |
| `streams_get_group` | A group's members, offsets and lag per partition. | `GET /v1/streams/{s}/groups/{g}` |
| `streams_branch` | A stream branched, records shared (its key's secret: reveal). | `POST /v1/streams/{s}/branches` |
| `streams_usage` | A stream's usage and throughput. | `GET /v1/streams/{stream}/usage` |
| `nosql_list_databases` | ScyllaDB/Cassandra-compatible databases. | `GET /v1/nosql/databases` |
| `nosql_get_database` | A database's settings and endpoints (credentials need reveal). | `GET /v1/nosql/databases/{database}` |
| `nosql_create_database` | A new database. | `POST /v1/nosql/databases` |
| `nosql_delete_database` | Delete a database and its data. Irreversible. | `DELETE /v1/nosql/databases/{database}` |
| `nosql_create_branch` | A branch database, now or at a point in time. | `POST /v1/nosql/databases/{database}/branches` |
| `nosql_cql` | Run a CQL statement. SELECT reads; DROP and TRUNCATE delete; the rest write. | `POST /v1/nosql/databases/{database}/cql` |
| `nosql_schema` | Keyspaces, tables and their columns. | `GET /v1/nosql/databases/{database}/schema` |
| `nosql_usage` | A database's usage report. | `GET /v1/nosql/databases/{database}/usage` |
| `nosql_reset` | A branch back to its parent's state, in place. | `POST /v1/nosql/databases/{id}/reset` |
| `nosql_restore` | Rewind a database in place to a point in time. | `POST /v1/nosql/databases/{id}/restore` |
| `nosql_diff` | What a branch changed against its parent. | `GET /v1/nosql/databases/{id}/diff` |
| `queues_overview` | The broker's totals and listeners. | `GET /v1/queues/overview` |
| `queues_list_vhosts` | RabbitMQ-compatible virtual hosts. | `GET /v1/queues/vhosts` |
| `queues_create_vhost` | A new virtual host. | `POST /v1/queues/vhosts` |
| `queues_delete_vhost` | Delete a virtual host and everything in it. | `DELETE /v1/queues/vhosts/{vhost}` |
| `queues_list_queues` | A virtual host's queues with message counts. | `GET /v1/queues/vhosts/{vhost}/queues` |
| `queues_declare_queue` | Create a queue. | `POST /v1/queues/vhosts/{vhost}/queues` |
| `queues_delete_queue` | Delete a queue and its messages. | `DELETE /v1/queues/vhosts/{vhost}/queues/{queue}` |
| `queues_purge_queue` | Drop every message in a queue. | `POST /v1/queues/vhosts/{vhost}/queues/{queue}/purge` |
| `queues_peek` | Messages in a queue, without consuming them. | `GET /v1/queues/vhosts/{vhost}/queues/{queue}/messages` |
| `queues_publish` | Publish to an exchange with a routing key. | `POST /v1/queues/vhosts/{vhost}/publish` |
| `tasks_publish` | Deliver an HTTP request to a URL, topic or queue, now or later, with retries. | `POST /v1/tasks/publish` |
| `tasks_list_messages` | Messages by state, queue or topic. | `GET /v1/tasks/messages` |
| `tasks_get_message` | A message and its delivery attempts. | `GET /v1/tasks/messages/{id}` |
| `tasks_cancel_message` | Cancel a message (purge=true deletes it). | `DELETE /v1/tasks/messages/{id}` |
| `tasks_list_schedules` | Cron schedules. | `GET /v1/tasks/schedules` |
| `tasks_create_schedule` | A cron schedule that publishes a task. | `POST /v1/tasks/schedules` |
| `tasks_delete_schedule` | Delete a schedule. | `DELETE /v1/tasks/schedules/{id}` |
| `tasks_list_runs` | Durable function runs. | `GET /v1/tasks/runs` |
| `tasks_get_run` | A run with its steps. | `GET /v1/tasks/runs/{id}` |
| `tasks_invoke` | Start a durable function run. | `POST /v1/tasks/functions/{id}/invoke` |
| `tasks_retry_message` | Deliver a failed message again. | `POST /v1/tasks/messages/{id}/retry` |
| `tasks_retry_dead_letters` | Dead letters delivered again: some, a queue's, or all. | `POST /v1/tasks/dlq/retry` |
| `tasks_cancel_run` | Stop a durable function run. | `POST /v1/tasks/runs/{id}/cancel` |
| `tasks_rerun` | A past run's function run again with its input. | `POST /v1/tasks/runs/{id}/rerun` |
| `tasks_list_functions` | Durable functions. | `GET /v1/tasks/functions` |
| `tasks_list_queues` | Queues with their parallelism, rate and backlog. | `GET /v1/tasks/queues` |
| `tasks_stats` | Nodes, queue stats, dead letters, schedules and functions. | `GET /v1/tasks/stats` |
| `search_list_clusters` | Elasticsearch-compatible search clusters. | `GET /v1/search/clusters` |
| `search_get_cluster` | A cluster's settings, state, indices, size, URL and branches. | `GET /v1/search/clusters/{cluster}` |
| `search_create_cluster` | A new cluster, or a branch of one (its password once, to tokens that may reveal). | `POST /v1/search/clusters` |
| `search_branch_cluster` | A copy-on-write branch of a cluster. | `POST /v1/search/clusters/{cluster}/branches` |
| `search_update_cluster` | A cluster's idle timeout, request limits, flavor. | `PATCH /v1/search/clusters/{id}` |
| `search_delete_cluster` | Delete a cluster (its branches keep working). | `DELETE /v1/search/clusters/{id}` |
| `search_usage` | A cluster's metering and cost against Elastic Cloud and Amazon OpenSearch. | `GET /v1/search/clusters/{cluster}/usage` |
| `search_get_index` | An index's mappings, settings and aliases. | `GET /v1/search/clusters/{cluster}/indices/{index}` |
| `search_create_index` | Create an index with mappings and settings. | `PUT /v1/search/clusters/{cluster}/indices/{index}` |
| `search_delete_index` | Delete an index and its documents. | `DELETE /v1/search/clusters/{cluster}/indices/{index}` |
| `search_put_mapping` | Add fields to an index's mapping. | `PUT /v1/search/clusters/{cluster}/indices/{index}/mapping` |
| `search_get_mapping` | An index's field mappings. | `GET /v1/search/clusters/{cluster}/indices/{index}/mapping` |
| `search_index_settings` | An index's settings. | `GET /v1/search/clusters/{cluster}/indices/{index}/settings` |
| `search_refresh` | Make what was just indexed searchable now. | `POST /v1/search/clusters/{cluster}/indices/{index}/refresh` |
| `search_analyze` | The tokens an index's analyzer makes of some text (read). | `POST /v1/search/clusters/{cluster}/indices/{index}/analyze` |
| `search_add_document` | Index a document under a new id. | `POST /v1/search/clusters/{cluster}/indices/{index}/docs` |
| `search_bulk` | Index, update or delete many documents (NDJSON); deleting is delete. | `POST /v1/search/clusters/{cluster}/indices/{index}/bulk` |
| `search_delete_by_query` | Delete every document that matches a query. | `POST /v1/search/clusters/{cluster}/indices/{index}/delete_by_query` |
| `search_list_indices` | A cluster's indices. | `GET /v1/search/clusters/{cluster}/indices` |
| `search_query` | Run an Elasticsearch _search body on an index. | `POST /v1/search/clusters/{cluster}/indices/{index}/search` |
| `search_get_document` | A document by id. | `GET /v1/search/clusters/{cluster}/indices/{index}/docs/{doc}` |
| `search_index_document` | Create or replace a document. | `PUT /v1/search/clusters/{cluster}/indices/{index}/docs/{doc}` |
| `search_delete_document` | Delete a document. | `DELETE /v1/search/clusters/{cluster}/indices/{index}/docs/{doc}` |
| `secrets_list_environments` | A project's secret environments. | `GET /v1/secrets/projects/{project}/environments` |
| `secrets_list` | Secret names and metadata in an environment (never values). | `GET /v1/secrets/projects/{project}/environments/{env}/secrets` |
| `secrets_reveal` | A secret's value. | `GET /v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value` |
| `secrets_set` | Create or change a secret's value. | `PUT /v1/secrets/projects/{project}/environments/{env}/secrets/{name}` |
| `secrets_delete` | Delete a secret. | `DELETE /v1/secrets/projects/{project}/environments/{env}/secrets/{name}` |
| `secrets_list_aws_roles` | A project's registered AWS roles for dynamic AWS keys (registering is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/aws-roles` |
| `secrets_list_outside_databases` | Postgres servers roll doesn't run whose roles a project rotates (never the admin URL; adding one is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/outside-databases` |
| `secrets_list_aws_users` | A project's AWS IAM users whose keys roll rotates (never a key; adding one is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/aws-users` |
| `secrets_list_cloud_accounts` | A project's GCP and Azure accounts for dynamic credentials (no secrets; adding one is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/cloud-accounts` |
| `secrets_list_cas` | A project's own certificate authorities (public certificates, domains, limits; never a key; adding one is admin). | `GET /v1/secrets/projects/{project}/cas` |
| `secrets_list_ssh_cas` | A project's SSH CAs (public keys, principals, limits; never a private key; adding one is admin). | `GET /v1/secrets/projects/{project}/ssh-cas` |
| `secrets_changes` | What changed in an environment (who, when, which keys; never values). | `GET …/environments/{env}/changes` |
| `secrets_versions` | A secret's history (fingerprints, never values). | `GET …/environments/{env}/secrets/{name}/versions` |
| `secrets_compare` | Which keys differ between two environments (never values). | `GET /v1/secrets/projects/{project}/compare` |
| `secrets_rotate` | A new value for a secret with a rotator, switched to. | `POST …/environments/{env}/secrets/{name}/rotate` |
| `secrets_promote` | Copy another environment's secrets in (a change request into a protected one). | `POST …/environments/{env}/promote` |
| `secrets_list_requests` | An environment's change requests. | `GET …/environments/{env}/requests` |
| `secrets_propose` | Propose changes for a person to review and apply: the way to change a protected environment. | `POST …/environments/{env}/requests` |
| `secrets_transit_encrypt` | Encrypt with a project's encryption key (the key never leaves roll). | `POST /v1/secrets/projects/{project}/transit/{name}/encrypt` |
| `secrets_transit_decrypt` | Decrypt a `roll:v<n>:` (or Vault's `vault:v<n>:`) ciphertext; reveal, audited. | `POST /v1/secrets/projects/{project}/transit/{name}/decrypt` |
| `secrets_transit_sign` | An Ed25519 signature by a project's signing key. | `POST /v1/secrets/projects/{project}/transit/{name}/sign` |
| `secrets_transit_verify` | Whether a signature is the key's. | `POST /v1/secrets/projects/{project}/transit/{name}/verify` |
| `secrets_transit_public_key` | A signing key's public keys (raw, PEM, JWK). | `GET /v1/secrets/projects/{project}/transit/{name}/public` |
| `secrets_reach` | Who reaches a folder or key, where it flows, who read it (names only; admin): after a leak. | `GET /v1/secrets/projects/{project}/environments/{env}/reach` |
| `secrets_folder_policies` | An environment's approval policies per folder. | `GET /v1/secrets/projects/{project}/environments/{env}/folder-policies` |
| `secrets_list_rotators` | Which secrets roll rotates, how often. | `GET /v1/secrets/projects/{project}/rotators` |
| `secrets_list_syncs` | Where a project's environments are pushed. | `GET /v1/secrets/projects/{project}/syncs` |
| `secrets_diff` | A local .env against an environment, by fingerprint (names only). | `POST /v1/secrets/projects/{project}/environments/{env}/diff` |
| `secrets_list_rotations` | A project's secret rotations: what rotated and what's due. | `GET /v1/secrets/projects/{project}/rotations` |
| `secrets_ca_certificates` | The certificates a project's CA has issued (never a key). | `GET /v1/secrets/projects/{project}/cas/{ca}/certificates` |
| `secrets_health` | What needs fixing in a project's secrets, by name, in the environments you may read (`code`: keys no repository mentions). | `GET /v1/secrets/projects/{project}/health` |
| `secrets_findings` | Every project's secrets health findings you may read. | `GET /v1/secrets/findings` |
| `secrets_find` | Where a key is set across the projects you may read. | `GET /v1/secrets/search` |
| `secrets_scan` | Which of these values are a project's stored secrets (never the value): check a diff before committing it. | `POST /v1/secrets/projects/{project}/leaks` |
| `apps_list_services` | App services. | `GET /v1/apps/services` |
| `apps_create_service` | A new service in a project, from a repo, an image or an upload. | `POST /v1/apps/projects/{project}/services` |
| `apps_list_environments` | A service's environments. | `GET /v1/apps/services/{service}/environments` |
| `apps_get_service` | A service with its environments' summaries. | `GET /v1/apps/services/{service}` |
| `apps_set_environment` | Create or change an environment: branch, run settings, following an image tag, waiting for CI. | `PUT /v1/apps/services/{service}/environments/{env}` |
| `apps_update_service` | Change a service's name, source, build or run settings (run takes `alerts {memory_pct, error_pct, p95_ms}`). | `PATCH /v1/apps/services/{service}` |
| `apps_deploy` | Deploy a service to an environment: a git revision of its repo, or an image (production deploys are "deploy" actions). | `POST /v1/apps/services/{service}/environments/{env}/deploys` |
| `apps_list_deploys` | An environment's deploys, newest first. | `GET /v1/apps/services/{service}/environments/{env}/deploys` |
| `apps_deploy_logs` | A deploy's build and release log. | `GET /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/logs` |
| `apps_promote` | Deploy another environment's running image into this one. | `POST /v1/apps/services/{service}/environments/{env}/promote` |
| `apps_cancel_deploy` | Stop a deploy that hasn't gone live. | `POST /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/cancel` |
| `apps_sleep` | Stop an environment's instances until woken. | `POST /v1/apps/services/{service}/environments/{env}/sleep` |
| `apps_wake` | Start a sleeping environment's instances. | `POST /v1/apps/services/{service}/environments/{env}/wake` |
| `apps_usage` | An environment's usage and cost per day. | `GET /v1/apps/services/{service}/environments/{env}/usage` |
| `apps_restart` | Restart an environment's instances. | `POST /v1/apps/services/{service}/environments/{env}/restart` |
| `apps_restart_instance` | Replace one instance, a fresh one answering first (production: deploy). | `POST /v1/apps/services/{id}/environments/{env}/instances/{name}/restart` |
| `apps_redeploy` | Deploy what's running again with the current settings and variables. | `POST /v1/apps/services/{service}/environments/{env}/redeploy` |
| `apps_run_job` | Run a one-off command in an environment's image with its variables. | `POST /v1/apps/services/{service}/environments/{env}/jobs` |
| `apps_job_logs` | A job's output. | `GET /v1/apps/services/{service}/environments/{env}/jobs/{job}/logs` |
| `apps_events` | What happened to services, in order after a cursor. | `GET /v1/apps/events` |
| `apps_list_variables` | An environment's variables, values masked. | `GET /v1/apps/services/{service}/environments/{env}/variables` |
| `apps_set_variables` | Set or unset an environment's variables (production is a "deploy" action). | `PATCH /v1/apps/services/{service}/environments/{env}/variables` |
| `apps_metrics` | CPU, memory and requests of an environment. | `GET /v1/apps/services/{service}/environments/{env}/metrics` |
| `apps_plan` | How roll would build and run a repo, before making a service (a dry run). | `POST /v1/apps/plan` |
| `apps_get_environment` | One environment: URL, state, live deploy, branch, size. | `GET /v1/apps/services/{id}/environments/{env}` |
| `apps_delete_environment` | Delete an environment with its deploys and domains. | `DELETE /v1/apps/services/{id}/environments/{env}` |
| `apps_link` | Give a service a roll resource's connection variables (reveal on the resource). | `PUT /v1/apps/services/{id}/links/{kind}/{ref}` |
| `apps_unlink` | Stop giving a service a linked resource's variables. | `DELETE /v1/apps/services/{id}/links/{kind}/{ref}` |
| `apps_verify_domain` | Check a custom domain's DNS now. | `POST /v1/apps/services/{id}/environments/{env}/domains/{host}/verify` |
| `apps_list_backups` | An environment's volume backups. | `GET /v1/apps/services/{id}/environments/{env}/volume/backups` |
| `apps_backup_volume` | Back up a volume now. | `POST /v1/apps/services/{id}/environments/{env}/volume/backups` |
| `apps_restore_backup` | Put a volume back as a backup had it (delete: it replaces the files). | `POST /v1/apps/services/{id}/environments/{env}/volume/backups/{bid}/restore` |
| `apps_activity` | Who changed what on a service (never a value). | `GET /v1/apps/services/{service}/activity` |
| `apps_get_deploy` | One deploy. | `GET …/environments/{env}/deploys/{deploy}` |
| `apps_list_links` | What a service is linked to. | `GET /v1/apps/services/{service}/links` |
| `apps_list_cron_runs` | A cron service's runs. | `GET …/environments/{env}/runs` |
| `apps_get_cron_run` | One cron run: status, exit code, times. | `GET /v1/apps/services/{id}/environments/{env}/runs/{run}` |
| `apps_cron_run_logs` | A cron run's output (read: its lines are in the environment's log). | `GET /v1/apps/services/{id}/environments/{env}/runs/{run}/logs` |
| `apps_cancel_cron_run` | Stop a running cron run (production: deploy). | `POST /v1/apps/services/{id}/environments/{env}/runs/{run}/cancel` |
| `apps_run_cron` | Run a cron job now (deploy in production). | `POST …/environments/{env}/runs` |
| `apps_limits` | A project's apps spend limits. | `GET /v1/apps/projects/{project}/limits` |
| `apps_pending` | What the next deploy changes: variables (names), run settings, project secrets. | `GET /v1/apps/services/{service}/environments/{env}/pending` |
| `apps_deploy_commits` | The commits a deploy brings over the one live before it. | `GET /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/commits` |
| `apps_rollback` | Roll an environment back to an earlier deploy, with the variables it went live with. | `POST /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollback` |
| `apps_rollback_previous` | Put back the deploy that was live before this one. | `POST /v1/apps/services/{service}/environments/{env}/rollback` |
| `apps_logs` | Recent logs of a service's environment, or with `http` its requests (filter by `status`, `min_ms`). | `GET /v1/apps/services/{service}/environments/{env}/logs` |
| `apps_list_domains` | An environment's domains. | `GET /v1/apps/services/{service}/environments/{env}/domains` |
| `apps_add_domain` | Point a domain at an environment. | `POST /v1/apps/services/{service}/environments/{env}/domains` |
| `apps_remove_domain` | Remove a domain. | `DELETE /v1/apps/services/{service}/environments/{env}/domains/{host}` |
| `previews_list` | Preview environments. | `GET /v1/previews` |
| `previews_get` | A preview with its branched resources and its schema changes. | `GET /v1/previews/{id}` |
| `previews_create` | A preview environment for a change or a branch: the project's resources branched, its services deployed. | `POST /v1/previews` |
| `previews_refresh` | Redeploy a preview now. | `POST /v1/previews/{id}/refresh` |
| `previews_logs` | A preview's deploy and service logs. | `GET /v1/previews/{id}/logs` |
| `previews_reset` | Branch a preview's resources again from their parents. | `POST /v1/previews/{id}/reset` |
| `previews_keep` | Keep a preview past its time to live, for some hours. | `POST /v1/previews/{id}/keep` |
| `previews_activity` | What happened to a preview, newest first. | `GET /v1/previews/{id}/activity` |
| `previews_share` | A link for someone outside the team, for some hours (reveal: its token is in the answer). | `POST /v1/previews/{id}/shares` |
| `previews_revoke_share` | Stop a preview's share link at once. | `DELETE /v1/previews/{id}/shares/{share}` |
| `previews_settings` | A project's preview rules (changing them is admin, in the dashboard). | `GET /v1/previews/projects/{project}/settings` |
| `previews_list_shares` | Who a preview is shared with outside the team (never the links' tokens). | `GET /v1/previews/{id}/shares` |
| `previews_env` | A preview's own variables, with their values (reveal). | `GET /v1/previews/{id}/env` |
| `previews_connect` | Each data branch's connection as the preview's services get it (the branches' own credentials; reveal). | `GET /v1/previews/{id}/connect` |
| `previews_list_mail` | The mail a preview's services sent, caught and never delivered (reveal). | `GET /v1/previews/{id}/mail` |
| `previews_read_mail` | One caught message as text (reveal). | `GET /v1/previews/{id}/mail/{mail}` |
| `previews_list_notes` | Notes left on a preview's pages that nobody has resolved yet (read). | `GET /v1/previews/{id}/notes` |
| `previews_resolve_note` | A note from the preview page's feedback button marked dealt with (write). | `POST /v1/previews/{id}/notes/{note}/resolve` |
| `previews_screenshot` | A preview's page as an image the agent can look at (`diff`, `page`, `view`); accepting a visual change stays a person's (admin, no tool). | `GET /v1/previews/{id}/screenshot` |
| `previews_sleep` | Stop a preview's services until it's woken. | `POST /v1/previews/{id}/sleep` |
| `previews_wake` | Start a sleeping preview's services again. | `POST /v1/previews/{id}/wake` |
| `previews_usage` | What a project's previews used and cost this month. | `GET /v1/previews/projects/{project}/usage` |
| `previews_delete` | Tear a preview down with its branches. | `DELETE /v1/previews/{id}` |
| `code_…`, `agents_…` | roll code's tools (docs/code/mcp.md), under product names | as there |

## Next

Ranked; the top item is in progress. A watchdog reads this list. Unblocked work comes first; what
waits on a sibling PR reaching main is last.

1. **Dry-merge the sibling heads after each of their pushes** that touches the gate, the catalogue or
   rolld, and send each thread its conflicts and their resolutions (`sibling-auth.sh`: every
   route classified, per method, with #25), and compare each product's routes with its tools
   (an everyday route with no tool is a gap: apps' were found this way).
2. **Integration with secrets, on `feature/mcp-on-secrets`** (#25 with secrets' swap, #30, merged;
   lands after #20, #25 and #30): kept merged both ways (secrets' head of 2026-11-14 merged: a
   forgotten project's credentials revoked first and retried by the sweeper, with the integration's
   `identity_logins` kept in what's forgotten). The canvas check costs nothing measurable
   (`gate_latency`, release, pinned, A/B twice: 83–115 µs p50 with it, 105–116 µs without).
3. *Waits on secrets' transit rows (#47, added by secrets where #47 and #24/#25 first meet):*
   `.index("name")` on the per-key routes and `Scope.key` in their own check (`GrantGrants` sets
   `id` since #30's fix). The tools and the builder's key picker are in; check both live once the
   rows land (a token on `prj_x/one-key` signs with it and is refused another key and the
   project's environments).
4. **The operator's steps:** only if a load balancer that terminates HTTP sits in front of rolld (not
   roll's own edge, which needs nothing), set `ROLL_TRUSTED_PROXIES` to its addresses; and register `rlt_`, `rlo_`, `rlf_` and `rlk_` (`[a-z2-7]{40}` with a
   checksum) with GitHub's secret scanning partner program, so leaks on GitHub reach
   `/v1/access/leaks/github`.
5. **Postgres e2e:** five history/replica e2e tests fail on this server because computes can't
   reach the daemon on the Docker bridge; run `pnpm test:pg` where they can (CI) before merging.

Considered and left out: a docs search tool (Supabase's `search_docs`): roll's docs are design docs (plans, `## Next` lists, test maps), and serving them would hand agents roadmap notes as how roll works; `roll://guide` covers using roll over MCP, until user-facing docs exist; a smaller default toolset, or search-then-call meta-tools (see Toolsets: clients handle long lists or have a picker, and meta-tools would hide each tool's hints); a connection narrowed to one project (Supabase's `project_ref`): their
tokens are account-wide; a roll token's rules already name projects, and the builder's presets
start from one. `outputSchema` per tool: it would double `tools/list` for clients that read
`structuredContent` as JSON anyway.

Done lately: a person's canvas no longer widens a project rule (`Workspace::trusts`, built on
secrets' branch) and people's tokens reach secrets; a person's pending token leads an admin's
checkup ("Needs attention": approve or deny), and stays out of the person's own (#25); admins are
mailed when a person's token waits for them (who asked, what it may do, the link to decide;
`Sessions::admins`, accounts' list) (#25); `roll token list --pending` and `roll token approve|deny
<id>` (#25); people's tokens past the policy wait for an admin (#25; GitHub's organization approval:
`approve_people`, 401 `pending_approval`, approve or deny on the access page, the person mailed,
widening waits again); the ninth review's findings (people's mail in roll's words only: names as
words, an approval's mail without its resource, a leak's without where, one approval mail per token
per ten minutes and 20 a person an hour; workload tokens capped per owner and a person's listed
before the limit; a refresh token's retry within its grace audited; apps settings that change what
production runs, its source, build, start and pre-deploy commands, schedule, kind, port, volume and
`run.public`, are deploy; `create_grant` refuses extensions that name no caller; the expiry digest
leaves out tokens made for a day or less); `secrets_reach` and `secrets_folder_policies` (secrets
#61, #53); rotating a token can keep the old secret working a while (`keep_old_for_s`, at most a
week: Stripe's roll key), from the dashboard's Rotate and `roll token rotate --keep-old 1h`, so
deployments switch without an outage; `postgres_typescript_types` (Supabase's generated types,
written by one read-only query: enums as unions, arrays, nullable) and the `compare_branch_schemas`
prompt (Neon's schema compare: both branches described, the difference listed, advisors on the
branch); open registration's limit is a sliding minute per network (a fixed minute let twice the
limit through at its edge, and its test failed when a run crossed one); a token used before its
start says so (401 `not_yet_valid`, "this token works from <time>", with `not_before`; the token
list and page show the start); a token can start later (`not_before`, Cloudflare's TTL start):
refused until then as an unknown token, the dashboard's "Starts" and `roll token create --starts 2h`
(the expiry counts from the start), changeable and clearable; a person's own token's page tested to
show only their reach; `roll token list --product/--resource` and `--workloads`; a workload token
makes no tokens; workload tokens (`Access::mint_workload`: minutes, within an identity's scope,
owned by its maker, out of the list and caps) for secrets' identity logins; a person's own tokens'
news is mailed to them (an approval waiting on them, used from a new network, revoked as leaked,
about to expire; only their own, never a secret); `previews_resolve_note` (a preview page's note,
write by the prefix; previews-next), and `apps_logs` takes `before`, `instance`, `deployment` and
`stream` (apps-next); Postgres analysis as Neon's and Supabase's MCP servers have it:
`postgres_explain`, `postgres_slow_queries`, `postgres_advisors`, `postgres_list_extensions`
(read-only SQL through the query route, checked on Postgres 18); names people choose can't hide
text: a token's name with invisible characters is refused, OAuth clients' names (registered or from
a metadata document) drop them, and `initialize`'s instructions escape them; `apps_set_environment`
takes response header rules (`headers`, apps-next); a cron run's logs are read again, as apps-next
decided (its lines were always in the read-level environment log); `apps_restart_instance`
(apps-next, reviewed under the environment prefix: deploy in production); invisible characters (tag
characters, zero-width and bidi controls, variation selectors) escaped in results' text, so
instructions hidden in other people's text stay visible to the model; plain-read answers checked for
credentials (key listings: streams' are usernames, storage's skip the secret, tasks' are hashes and
prefixes); the fifteenth review: a list's credentials redacted item by item (a token with reveal on
one Postgres database and read on all saw every database's password in `GET /v1/projects`; the same
for every list with credential fields), a preview's `share_url` redacted without reveal (its access
token: the QR code's reveal row protected nothing), and a cron run's logs reveal on this branch too;
`previews_list_notes` and `previews_resolve_note` (a preview page's note, write by the prefix;
previews-next), and `apps_logs` takes `before`, `instance`, `deployment` and `stream` (apps-next);
apps-next's cron runs as tools (`apps_get_cron_run`, `apps_cron_run_logs`, `apps_cancel_cron_run`);
a preview's caught mail as reveal, with `previews_list_mail` and `previews_read_mail`
(previews-next); the connect panel says what a choice of toolsets costs (tools and context tokens,
from `GET /v1/mcp/toolsets`) and warns past a client's limit (VS Code refuses more than 128 tools a
request; with every product on roll lists about 259); the token page counts what a model reads
(name, description, schema); `previews_connect` and its reveal row, and a preview's QR code as
reveal (previews-next); the core products' everyday routes as tools (streams: a topic, a group's
lag, branching; tasks: retrying messages and dead letters, cancelling and rerunning runs, functions,
queues; NoSQL: reset, restore, a branch's diff; storage: branching a bucket, the trash and restoring
from it); against Neon's MCP server and main's routes: `postgres_reset_branch` (reset from parent),
`postgres_get_endpoint`, `postgres_update_endpoint`, `redis_restore`, `apps_delete_environment`;
each product's routes compared with its tools (`tool-gaps.py`): `search_update_cluster`,
`search_delete_cluster`, `secrets_list_rotators`, `secrets_list_syncs` and `secrets_diff` added;
previews' bypass, search's keys, password and console, and secrets' bulk values, reviews and
settings stay a person's; apps' missing everyday tools (`apps_create_service` takes links and run
settings, an environment's state, linking and unlinking resources, checking a domain, volume backups
and restoring one; all on main); a tool any token may call (`apps_plan`) is listed only where its
product runs (it showed with apps off); previews' tools listed (they never were: roll previews runs
with roll code and has no product switch, so MCP's product list left it out; rolld's auth test now
fails on any product it serves with no tools listed); the token builder offers only the products
this rolld runs (and any a token's rules name already), not all sixteen; checked live on #25's build
(both official SDKs end to end, progress during an approval, URL elicitation; `serverInfo.icons`
answered), which found roll code's fronted tools still carrying the approval argument for the
operator (37 of 78): they drop it too now; the fourteenth review (key-scoped rules, before any row
uses them): secrets' key names are literal (a key named `_all` is only that key; a list or a `*` in
a request names none) and an approval remembers only a request that named one key; a pending answer
says the call finds its approval without the argument too; the approval argument only where a person
can be asked (a model reads every tool's schema: 92 → 73 KB for the operator and for tokens that
only allow, every product on; `tools/list` 140 → 120 KB, 190 tools); the builder lists a secrets
project's encryption keys to tick (one picker with search's indices; secrets' projects come from the
workspace); encryption keys as MCP tools (`secrets_transit_encrypt`, `_decrypt`, `_sign`, `_verify`,
`_public_key`; secrets #47: their rows narrow to one key); roll's mark as the server's icon
(`serverInfo.icons`); a rule can narrow a secrets project to some of its encryption keys (`{id,
index}`, as search's indices: signing with one key needn't be write on the project; secrets' transit
rows add `.index("name")`); `secrets_list_rotations` and `secrets_ca_certificates` (secrets #48,
#44); `tools/list` 11% smaller with every product on (183 tools, 150 → 134 KB: the approval
argument's description shortened, the title not repeated in annotations); apps' plan, activity,
deploys, links, cron runs and limits as MCP tools (apps-next #26; the shell, tunnel, hooks and
webhook notifications stay out); secrets' history and workflow as MCP tools (changes, versions,
compare, rotate, promote, change requests and proposing one: how an agent changes a protected
environment); `previews_revoke_share` and `previews_settings` (previews-next #28); `apps_pending`
and `apps_deploy_commits` (what the next deploy changes, what a deploy brought; apps-next #26), and
`debug_failing_service` uses them; `roll token tools <id>` (what an agent sees, from the CLI); roll
code's stream and device-login caps take the client the trusted proxy saw (the right-most
`X-Forwarded-For` entry that isn't the proxy, over every line, only an address), not the left-most a
client wrote; apps-next's head passes the per-method check with #25; the thirteenth review
(X-Forwarded-For only from listed proxies, every line, a bad entry stopping the walk, IPv4-mapped
addresses matched; nothing listed, the header counts for nothing; MCP tool calls carry the gate's
address; unconsented client ids good for 90 days and kept ids small enough for Postgres);
`ROLL_TRUSTED_PROXIES` (X-Forwarded-For only from the edge's addresses, the client the right-most
entry that isn't one: a direct request can't name its own address for IP allowlists or
registration's limit); sealed apps variables explained to agents (apps-next #26); the prefix check
per method (rolld logs each route's methods; 194 `METHOD route` lines reviewed, 7 newly found and
looked at: a route with its own row for one method can't gain another a prefix decides unseen);
sensitive secrets explained to agents (`secrets_reveal` and `roll://guide`: a 403 and `withheld` are
expected; secrets #46); open registration stores nothing (a client's id is the client, signed; kept
once a person consents, else good for a day), so a flood has nothing to fill; the twelfth review's
contained findings (CSV cells all quoted with formulas defused after in-cell separators too; a
completion never waits on or uses an open approval; registration's rate map swept once a minute and
bounded; the token-page list read from the caller's address and off the async threads; stored grants
honour the idle policy; client names as consent shows them, no invisible characters); a person's own
token's page tested to show only their reach (#25); `secrets_list_ssh_cas` (secrets #45); open
client registration bounded (20 a minute a network; a flood never pushes out a client a person is
connecting); `secrets_list_cas` (secrets #44); the prefix check skips public routes (no grant
classifies what anyone may call); `secrets_list_cloud_accounts` (secrets #43); the audit log as CSV
(the audit page, `roll mcp audit --format csv`; formula-safe cells); what an agent sees, on the
token page (`GET /v1/access/tokens/{id}/tools`: the tools and prompts a token is offered and what
the list costs an agent's context); completions named as such in the audit log
(`completion/complete:<tool>`); a connection's tool names worked out once per request; every open
sibling's head passes the route check with #25 (apps-next's rollback to the last deploy,
previews-next's share revoke, search's index refresh and settings reviewed: 178 in all);
`secrets_list_aws_users` (secrets #40); `apps_set_environment` takes who gets in (maintenance,
allowed addresses, a rate limit, firewall rules; apps-next #26); routes a prefix row decides are
reviewed (`prefix-reviewed.txt`, 138 looked at; rolld's auth test fails on a new one until it has
its own row or is listed); previews-next's routes (#28): a preview's env is reveal, accepting a
visual change is admin with no tool, screenshots come back as MCP image content
(`previews_screenshot`), `previews_list_shares`, `previews_env`; the eleventh review (MCP's new
surface): read-only excludes reveals that make something (a presigned PUT, a share link), either
switch turns it on and a misspelled key is a 400; completions offer only ids and never ask a person;
prompts' arguments are quoted ids; template errors say what they were;
`secrets_list_outside_databases` (secrets #39); a project's page as a resource template
(`roll://projects/{project}`, read through the gate, its id completed); progress while a call waits
(`notifications/progress` every 10 s for a call with a `progressToken`, checked with the TypeScript
SDK; both SDKs' end-to-end runs pass); `secrets_list_aws_roles` (secrets #38); read-only connections
(`?readonly=1`: only read-only tools listed and run, a checkbox in the connect panel);
`try_migration_on_branch` (a schema change on a Postgres branch first, main only when it worked and
you say so); prompts' arguments complete from what the connection may list (`completion/complete`,
through the gate); MCP prompts (roll's workflows as slash commands: what this connection may do,
checking a diff for secrets, fixing secrets health, debugging a failing service; offered only with
their tools); the integration branch on Postgres 18 (roll-access, roll-mcp, roll-workspace,
roll-secrets) and live (a person's own token over MCP reads their own secrets project, the
operator's answers 404, disabled it stops; `users-live/secrets.sh`); the tenth review (access with
secrets): a person's stored grant dead for products' background work once they can't sign in
(`Sessions::person_now`), hops carrying only their own product's routes, `Access::revoke_workload`;
apps' rollbacks as tools say they put back a deploy's variables (`keep_variables`), and
`apps_rollback_previous` (apps-next #26); every open sibling's routes classified, built with #25
(apps-next #26, previews-next #28 and search #29 pass rolld's route test); a preview's share link is
reveal (`previews_share`); search's index mapping, settings, refresh and analyze as MCP tools;
`Access::create_grant` (a product makes its own tokens for its caller through the token route's own
function) and tokens listed by what they were made for (`?product=&resource=`), for secrets retiring
`rlst_`; taking an apps web service off the internet or putting it on (`run.public`) is deploy; a
token's rate is the fleet's (each node enforces its share, the rate over the live nodes), not N
times it; a worker's scaling stream needs read on it; a spent OAuth refresh token used again revokes
the connection (OAuth 2.1; 30 seconds of grace for a retry); only token and access secrets sign a
request in.

Done before (oldest last): the consent screen says what a client's identity is worth (a metadata
document's address vouches for it; a registered name doesn't); the access page's checkup and
toolsets have a component test; `roll token checkup` prints each fix's command; `tools/list` ~13×
faster (149 tools, release: 43 → 3.2 ms p50, 90 → 6.3 ms p99; each tool's actions and definition are
worked out once instead of matching every catalogue row three times per tool per call;
`tools_list_cost`, an ignored test, measures it); secrets' diff is reveal; the access checkup (which
tokens need attention and why: admin over everything, never expiring, unused, narrower than their
rules, roaming; each fix one click away); the eighth review's findings (audit batches written in one
statement and row by row when one fails, NULs dropped, a smaller queue that drains inside sinks'
settling time, flushed at shutdown; names in chat notices plain text and Slack text escaped; IPv4 on
dual-stack listeners its own network; one alert per new network across nodes; client names without
bidi characters and shown as what the client says; approving together only what's shown and never
production; the resource filter exact; networks pruned); apps' `rlk_` deploy hook secrets found and
revoked like tokens; a token's waiting reads and writes approved together from the header (never
what's risky or has a body to review); a token used from a new network is told where approvals go
(/24s and /48s, until it roams at five), and its page lists its networks; secrets' health, findings,
find and scan as MCP tools (an agent checks its diff before committing it); which client made a
request (an MCP client's name and version from initialize, carried in the session id, else the
User-Agent), in the audit log and the token page; all of an apps shell session is admin and waking
an environment is write; the audit log pruned 5,000 rows at a time; a token's requests ~12× the
throughput on a node (9k → 110k a second with 32 callers, release): audit rows go through one writer
that batches what's queued into a transaction, every row kept (checked on SQLite and Postgres 18);
the gate about twice as fast (release, in process, the same instrumentation before and after: a
token's request on one resource 355 → 163 µs p50, 727 → 433 µs p99; the catalogue is indexed by
shape once instead of every row's shape being built per request, and last use is claimed in memory
before any write; what's left is mostly the token lookup, ~67 µs, never cached; `gate_latency`, an
ignored test in roll-access, measures it); who touched this (the audit log by resource, with what's
under it, and by action; one click from any row; `roll mcp audit --resource --action`); toolsets
(`?toolsets=` lists only some products' tools: Redis alone ~2.4k tokens instead of ~16k; the connect
panel picks them; OAuth checked with the SDK on such a URL); the seventh review's findings (nothing
filters itself until secrets reads the caller's Grant, and secrets' cross-project routes need a rule
for all of secrets meanwhile; approvals on such routes and on several links are approved once; take
back checks the token and counts only live rules; links name resolved ids; second checks are audit
rows of their own; tighten applies over the rules it was shown from); apps' volume backups; a link
needs reveal on what it links (apps' links hand a database's credentials to a service; the approval
is about the database); secrets' health and leak scan reach environment-limited tokens, filtered by
secrets; `roll token tighten` and `roll token take-back`; rules for a while name their approval and
can be taken back from the token's page (the unused approval with them); "Tighten to what it used"
(a token's rules narrowed to the actions and resources its allowed requests used, never wider,
applied from its page); a search cluster's indices ticked by name in the builder, and approvals
showing the environment and folder they're about; the sixth review's findings (a copy's approval
names the source folder, strict selectors, denials answered again, pushes bound to their parsed ref
updates, failed source lookups deny, parameters the gate reads can't repeat, unique session ids,
URL-elicitation clients answered at once); `apps_update_service`; secrets' leak scan; approvals as
URL elicitation (a client declaring `elicitation.url` gets `-32042` with the approval page;
`Mcp-Session-Id` carries only that) and the same request finding its approval again (no header
needed, one approval per request), checked live with the official TypeScript SDK; `apps_logs` reads
request logs (`http`, `status`, `min_ms`); a Postgres branch's app role; search indices as resources
(`{id, index}` selectors covered name by name and pattern by pattern; the cluster's own routes stay
the cluster's; remembered approvals keep their indices); secrets' promote, imports (per source),
backup, restore and rotators classed as secrets classes them; the fifth review's findings (what
agents read is quoted, a person's token's reach is stated, approvals filtered by token in the query,
the token page honest about dead tokens and failed refreshes); MCP resources (`roll://access/me`:
what this connection may do in words, read through the gate; `roll://guide`); apps' log drains in
the catalogue (adding one is reveal); a page per token (`/dashboard/access/tokens/<id>`: its rules
and its rules for a while in words, its approvals and recent requests, edit, rotate and revoke;
names in the list link to it); the fourth review's findings (a copy's approval names the source
environment, the apps shell is admin everywhere, use recorded only from allowed addresses, `check`
decides as the gate, the popover shows details and sends bodies to review); `Resolvers::by_fn` /
`environment_fn` (a product's lookups as one-line closures), and every sibling head dry-merged with
this branch (conflicts and their resolutions sent to each thread); approvals decided from the top
bar's waiting badge (approve or deny in a popover) and a request's arguments shown readably (a
push's ref updates, SQL, CQL or a Redis command as code); MCP tools for the sibling branches' routes
at their heads (search: clusters, indices, documents, bulk; apps: create a service, promote,
environments, sleep and wake, usage, cancel; previews: keep, activity), and search's bulk decided by
its lines; the builder picks resources by name (each product's "Which": all, the project, or ticked
resources) and says in words what the token can do and what happens to the rest; the evaluator's
suites run in `pnpm test:code` (on Postgres with `--docker`) and CI; "ask for anything else"
(`ask_otherwise`: what a token's rules don't allow asks a person, and remembered approvals grow it);
hop hardening for users' own tokens (the carried route is judged too, on their branch); copying
another environment's variables needs reveal there; the third security review's findings (remembered
approvals keep their environment and path, every MCP tool call counts against a rate limit, GitHub
key fetching single-flight with fallback, the leak scanner skips unreadable files under a budget and
hides private repos, per-person and live-only token caps, admin tokens owned by the admin); tokens
about to expire are named in the list and posted to the approvals channel daily; `apps_deploy` takes
an `idempotency_key` (sent as `Idempotency-Key`); an idle limit in the token policy
(`max_idle_days`: unused tokens stop); remembered approvals (docs/permissions.md: approve for an
hour, a day or always, one action on one resource, a while never made permanent through a child
token); leaked tokens revoke themselves (docs/permissions.md, "Leaked tokens": checksummed secrets,
roll code's landings scanned from the land log, GitHub's secret scanning partner endpoint); audit
sinks (docs/permissions.md, "Audit sinks": the log streamed to a SIEM in signed batches, the same
contract as roll secrets' sinks, checked live against a receiver that verifies signatures);
per-token rate limits (docs/permissions.md, "Rate limits": GCRA per grant per node, `429` with
`Retry-After` and `RateLimit-*`, an MCP message counted once, never above the maker's); the official
TypeScript (`@modelcontextprotocol/sdk` 1.32.2, OAuth included) and Python (`mcp` 2.3.0) SDKs drive
a live rolld end to end; the only thing they tripped on, registration answering `"client_uri":
null`, is fixed.
