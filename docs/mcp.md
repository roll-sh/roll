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
call once it's approved. An approval runs exactly the request it was asked for (same method, path
and body), once, and asking the same thing again waits on the same approval rather than making
another. `access_wait_approval` waits for a decision without running anything. A denied call
fails with the user's reason.

A client that opens URLs for elicitation (it declares `capabilities.elicitation.url` in
`initialize`, MCP 2025-11-25) gets the approval as one instead: the call answers the JSON-RPC error
`-32042` with `data.elicitations[0]` = `{mode: "url", elicitationId: "apr_…", url, message}` (and
`data.approval`). The client shows the person the approval page, and sends the call again once
they've decided; the gate finds its approval.

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
  `initialize` answers an `Mcp-Session-Id` that holds no state: it only says whether the client
  declared URL elicitation (`roll-url-1` or `roll-1`), so later calls know how to ask for approvals.
  A request without one works as before. `GET` and `DELETE` answer 405.
- Responses are JSON, or server-sent events when `Accept` has `text/event-stream` without
  `application/json`. Keep-alive comments come every 15 seconds while a call waits, for example on
  an approval.
- Requests with an `Origin` header must come from the dashboard's or the API's origin (DNS
  rebinding).
- Results come back as `structuredContent` (the route's JSON; arrays as `{"items": […]}`) and a
  `text` block, cut at 256 KiB. Binary content comes back base64 (`content_b64`).
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
as a tool is, so it says what the gate decides and the read is in the audit log. There are no
prompts: nothing roll does needs a canned prompt the tools' descriptions don't already give.

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
| `postgres_query` | Run read-only SQL on an endpoint: it runs in a read-only transaction, so writes fail. Returns columns and rows. | `POST /v1/endpoints/{endpoint}/query` |
| `postgres_execute` | Run SQL that may change data or schema (INSERT, UPDATE, DDL) on an endpoint. | `POST /v1/endpoints/{endpoint}/query` |
| `postgres_describe_schema` | Every table's columns and types on an endpoint (read-only). | `POST /v1/endpoints/{endpoint}/query` |
| `postgres_start_endpoint` | Wake an endpoint's compute. | `POST /v1/endpoints/{endpoint}/start` |
| `postgres_suspend_endpoint` | Suspend an endpoint's compute. | `POST /v1/endpoints/{endpoint}/suspend` |
| `postgres_usage` | Compute and storage use of a project, with cost estimates. | `GET /v1/projects/{project}/usage` |
| `redis_list_databases` | Serverless Redis databases. | `GET /v1/redis/databases` |
| `redis_get_database` | A Redis database's settings and connection (credentials need reveal). | `GET /v1/redis/databases/{database}` |
| `redis_create_database` | A new serverless Redis database. | `POST /v1/redis/databases` |
| `redis_delete_database` | Delete a database and its data. Irreversible. | `DELETE /v1/redis/databases/{database}` |
| `redis_create_branch` | A fork of a database's data, now or at a point in time. | `POST /v1/redis/databases/{database}/branches` |
| `redis_command` | Run one command on a database, e.g. ["GET", "key"]. Read-only commands are reads; FLUSHDB and FLUSHALL delete. | `POST /v1/redis/databases/{database}/command` |
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
| `streams_usage` | A stream's usage and throughput. | `GET /v1/streams/{stream}/usage` |
| `nosql_list_databases` | ScyllaDB/Cassandra-compatible databases. | `GET /v1/nosql/databases` |
| `nosql_get_database` | A database's settings and endpoints (credentials need reveal). | `GET /v1/nosql/databases/{database}` |
| `nosql_create_database` | A new database. | `POST /v1/nosql/databases` |
| `nosql_delete_database` | Delete a database and its data. Irreversible. | `DELETE /v1/nosql/databases/{database}` |
| `nosql_create_branch` | A branch database, now or at a point in time. | `POST /v1/nosql/databases/{database}/branches` |
| `nosql_cql` | Run a CQL statement. SELECT reads; DROP and TRUNCATE delete; the rest write. | `POST /v1/nosql/databases/{database}/cql` |
| `nosql_schema` | Keyspaces, tables and their columns. | `GET /v1/nosql/databases/{database}/schema` |
| `nosql_usage` | A database's usage report. | `GET /v1/nosql/databases/{database}/usage` |
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
| `tasks_stats` | Nodes, queue stats, dead letters, schedules and functions. | `GET /v1/tasks/stats` |
| `search_list_clusters` | Elasticsearch-compatible search clusters. | `GET /v1/search/clusters` |
| `search_get_cluster` | A cluster's settings, state, indices, size, URL and branches. | `GET /v1/search/clusters/{cluster}` |
| `search_create_cluster` | A new cluster, or a branch of one (its password once, to tokens that may reveal). | `POST /v1/search/clusters` |
| `search_branch_cluster` | A copy-on-write branch of a cluster. | `POST /v1/search/clusters/{cluster}/branches` |
| `search_usage` | A cluster's metering and cost against Elastic Cloud and Amazon OpenSearch. | `GET /v1/search/clusters/{cluster}/usage` |
| `search_get_index` | An index's mappings, settings and aliases. | `GET /v1/search/clusters/{cluster}/indices/{index}` |
| `search_create_index` | Create an index with mappings and settings. | `PUT /v1/search/clusters/{cluster}/indices/{index}` |
| `search_delete_index` | Delete an index and its documents. | `DELETE /v1/search/clusters/{cluster}/indices/{index}` |
| `search_put_mapping` | Add fields to an index's mapping. | `PUT /v1/search/clusters/{cluster}/indices/{index}/mapping` |
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
| `apps_list_services` | App services. | `GET /v1/apps/services` |
| `apps_create_service` | A new service in a project, from a repo, an image or an upload. | `POST /v1/apps/projects/{project}/services` |
| `apps_list_environments` | A service's environments. | `GET /v1/apps/services/{service}/environments` |
| `apps_get_service` | A service with its environments and deploys. | `GET /v1/apps/services/{service}` |
| `apps_deploy` | Deploy a service to an environment: a git revision of its repo, or an image (production deploys are "deploy" actions). | `POST /v1/apps/services/{service}/environments/{env}/deploys` |
| `apps_list_deploys` | An environment's deploys, newest first. | `GET /v1/apps/services/{service}/environments/{env}/deploys` |
| `apps_deploy_logs` | A deploy's build and release log. | `GET /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/logs` |
| `apps_promote` | Deploy another environment's running image into this one. | `POST /v1/apps/services/{service}/environments/{env}/promote` |
| `apps_cancel_deploy` | Stop a deploy that hasn't gone live. | `POST /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/cancel` |
| `apps_sleep` | Stop an environment's instances until woken. | `POST /v1/apps/services/{service}/environments/{env}/sleep` |
| `apps_wake` | Start a sleeping environment's instances. | `POST /v1/apps/services/{service}/environments/{env}/wake` |
| `apps_usage` | An environment's usage and cost per day. | `GET /v1/apps/services/{service}/environments/{env}/usage` |
| `apps_restart` | Restart an environment's instances. | `POST /v1/apps/services/{service}/environments/{env}/restart` |
| `apps_redeploy` | Deploy what's running again with the current settings and variables. | `POST /v1/apps/services/{service}/environments/{env}/redeploy` |
| `apps_run_job` | Run a one-off command in an environment's image with its variables. | `POST /v1/apps/services/{service}/environments/{env}/jobs` |
| `apps_job_logs` | A job's output. | `GET /v1/apps/services/{service}/environments/{env}/jobs/{job}/logs` |
| `apps_events` | What happened to services, in order after a cursor. | `GET /v1/apps/events` |
| `apps_list_variables` | An environment's variables, values masked. | `GET /v1/apps/services/{service}/environments/{env}/variables` |
| `apps_set_variables` | Set or unset an environment's variables (production is a "deploy" action). | `PATCH /v1/apps/services/{service}/environments/{env}/variables` |
| `apps_metrics` | CPU, memory and requests of an environment. | `GET /v1/apps/services/{service}/environments/{env}/metrics` |
| `apps_rollback` | Roll an environment back to an earlier deploy. | `POST /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollback` |
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
| `previews_sleep` | Stop a preview's services until it's woken. | `POST /v1/previews/{id}/sleep` |
| `previews_wake` | Start a sleeping preview's services again. | `POST /v1/previews/{id}/wake` |
| `previews_usage` | What a project's previews used and cost this month. | `GET /v1/previews/projects/{project}/usage` |
| `previews_delete` | Tear a preview down with its branches. | `DELETE /v1/previews/{id}` |
| `code_…`, `agents_…` | roll code's tools (docs/code/mcp.md), under product names | as there |

## Next

Ranked; the top item is in progress. A watchdog reads this list. Unblocked work comes first; what
waits on a sibling PR reaching main is last.

1. **Approvals as URL elicitation** (in progress): MCP 2025-11-25's `URLElicitationRequiredError`
   (`-32042`, `data.elicitations[{mode: "url", url, elicitationId, message}]`) is how a stateless
   server tells the client to open a page and retry. Clients that declare `elicitation.url` get
   approvals that way (the client opens the approval page for the person), others keep the
   `approval_pending` result. The official TypeScript SDK parses it already.
2. **Blocked on sibling PRs reaching main:** wire previews', search's, apps' and secrets' lookups
   (`by_fn` / `environment_fn`, one line each; their branches have them ready); secrets' `Grant`
   swap; one sink module with secrets; apps' deploys by id; users' own tokens' PR (#18, #19 on main).
3. **Postgres e2e:** five history/replica e2e tests fail on this server because computes can't
   reach the daemon on the Docker bridge; run `pnpm test:pg` where they can (CI) before merging.

Done lately: search indices as resources (`{id, index}` selectors covered name by name and pattern by pattern; the cluster's own routes stay the cluster's; remembered approvals keep their indices); secrets' promote, imports (per source), backup, restore and rotators classed as secrets classes them; the fifth review's findings (what agents read is quoted, a person's token's reach is stated, approvals filtered by token in the query, the token page honest about dead tokens and failed refreshes); MCP resources (`roll://access/me`: what this connection may do in words, read through the gate; `roll://guide`); apps' log drains in the catalogue (adding one is reveal); a page per token (`/dashboard/access/tokens/<id>`: its rules and its rules for a while in words, its approvals and recent requests, edit, rotate and revoke; names in the list link to it); the fourth review's findings (a copy's approval names the source environment, the apps shell is admin everywhere, use recorded only from allowed addresses, `check` decides as the gate, the popover shows details and sends bodies to review); `Resolvers::by_fn` / `environment_fn` (a product's lookups as one-line closures), and every sibling head dry-merged with this branch (conflicts and their resolutions sent to each thread); approvals decided from the top bar's waiting badge (approve or deny in a popover) and a request's arguments shown readably (a push's ref updates, SQL, CQL or a Redis command as code); MCP tools for the sibling branches' routes at their heads (search: clusters, indices, documents, bulk; apps: create a service, promote, environments, sleep and wake, usage, cancel; previews: keep, activity), and search's bulk decided by its lines; the builder picks resources by name (each product's "Which": all, the project, or ticked resources) and says in words what the token can do and what happens to the rest; the evaluator's suites run in `pnpm test:code` (on Postgres with `--docker`) and CI; "ask for anything else" (`ask_otherwise`: what a token's rules don't allow asks a person, and remembered approvals grow it); hop hardening for users' own tokens (the carried route is judged too, on their branch); copying another environment's variables needs reveal there; the third security review's findings (remembered approvals keep their environment and path, every MCP tool call counts against a rate limit, GitHub key fetching single-flight with fallback, the leak scanner skips unreadable files under a budget and hides private repos, per-person and live-only token caps, admin tokens owned by the admin); tokens about to expire are named in the list and posted to the approvals channel daily; `apps_deploy` takes an `idempotency_key` (sent as `Idempotency-Key`); an idle limit in the token policy (`max_idle_days`: unused tokens stop); remembered approvals (docs/permissions.md: approve for an hour, a day or always,
one action on one resource, a while never made permanent through a child token); leaked tokens revoke themselves (docs/permissions.md, "Leaked tokens": checksummed
secrets, roll code's landings scanned from the land log, GitHub's secret scanning partner
endpoint); audit sinks (docs/permissions.md, "Audit sinks": the log streamed to a SIEM in
signed batches, the same contract as roll secrets' sinks, checked live against a receiver that
verifies signatures); per-token rate limits (docs/permissions.md, "Rate limits": GCRA per grant per node,
`429` with `Retry-After` and `RateLimit-*`, an MCP message counted once, never above the maker's);
the official TypeScript (`@modelcontextprotocol/sdk` 1.32.2, OAuth included) and
Python (`mcp` 2.3.0) SDKs drive a live rolld end to end; the only thing they tripped on,
registration answering `"client_uri": null`, is fixed.
