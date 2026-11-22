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
  reads (`read`, or `reveal`), and `destructiveHint` is true for `delete` and `deploy` routes, for
  every `DELETE` route whatever the catalogue calls it (a drain, a lease, a link), and for what
  removes or replaces something by other means (revoking a key, rotating a secret, arbitrary
  statements like `postgres_execute`). A tool that only removes what can be put back says so
  (`.undoable()`: a resource off a canvas, a deleted cluster restored); a test fails on a tool
  whose name says it deletes, removes, revokes, rotates or restores and is neither.
  `_meta["roll/actions"]` lists the actions.
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

Every product's tools are about 500 (roll's own and roll code's: ~370 KB, ~56k tokens of what a
model reads; `pnpm test:mcp-sweep` prints the count and size, `GET /v1/mcp/toolsets` each
product's), before an agent does anything. Apps is the largest (101 tools, ~14k tokens), then
secrets (94, ~12k); measured on main 2026-11-20 (1f29e2feb). Building that list takes 6.5 ms at p50 and 15.5 ms at
p99 in release (`tools_list_cost`, an ignored test: linear in the tools, 3.2 ms for 149), and a
client keeps it for a minute (`ttlMs`). Connect to `/v1/mcp?toolsets=postgres,code` (or send `X-MCP-Toolsets: postgres,code`) to
list only those products' tools, as GitHub's MCP server does; the access tools (approvals,
`access_check`) are always listed. Redis alone is 10 tools and the access tools 11, ~2.2k
tokens instead of ~56k. A product too big for some clients on its own splits into areas, as GitHub's server splits its
toolsets: apps is `apps.observe` (logs, metrics, errors, traces, vitals, SLOs, usage, push,
events: 21 tools, ~3.2k tokens), `apps.deploy` (deploys, promotions, rollbacks, jobs, cron runs,
restarts: 39, ~5.9k), `apps.configure` (services, environments, variables, domains, volumes,
forms, links, runtime config: 30, ~5.5k) and `apps.project` (drains, notifications, limits, the
status page, the preview domain: 29, ~3.9k); secrets is `secrets.values` (keys, folders,
environments, changes, change and value requests, promotion, imports: 49, ~8.5k),
`secrets.security` (audit, findings, leaks, reach, insights, shares, rotations, the backup
schedule: 15, ~2.2k) and `secrets.integrations` (transit, leases, CAs, clouds, syncs and the other
outside systems: 36, ~4.8k). The lookups every area needs (apps' overview, services and environments; secrets' project and
environments) are in each. An area comes from its tool's route (`tools::apps_area`, `tools::secrets_area`), and a test
fails on a route of theirs it doesn't place; `GET /v1/mcp/toolsets` answers each area's cost under `areas`. An
unknown name is `400`, naming the toolsets and areas there are. The dashboard's connect
panel picks them: a product's chip, and for a product with areas a row of them (apps: All,
Observe, Deploy, Configure, Project; secrets: All, Values, Security, Integrations) once it's picked. A toolset only narrows the list: the token's rules decide every call as before,
and OAuth's resource is the server whatever the toolsets (checked with the TypeScript SDK).

Clients cap how many tools they take. VS Code refuses a chat request with more than 128 tools, its
own built-in ones included ("Cannot have more than 128 tools per request"); Cursor is reported to
drop tools past about 40; Windsurf documented a limit of 100 (its docs now live under Devin's
desktop app, whose MCP configuration states none; checked 2026-11-20). Claude Code defers a long
list behind its own tool search. So with every product on, VS Code and Cursor need toolsets; apps
(101 tools) and secrets (94) with the access tools each come near VS Code's 128 and far past
Cursor's 40, so there an area of each (or search mode) is the way in: the connect panel
adds up a choice (`GET /v1/mcp/toolsets`: each product's tools, read-only tools and context cost,
to a caller) and warns past the picked client's limit.

The whole list stays the default. A smaller default would quietly drop tools from connections
that use them.

### Search mode

With every product on, a node lists about 500 tools (~56k tokens), past VS Code's
and Cursor's limits. `/v1/mcp?tools=search` (or `X-MCP-Tools: search`) lists the access tools plus three
tools instead, about 1.8k tokens whatever runs (the three are ~500). This is the pattern of Cloudflare's code mode and
GitHub's dynamic toolsets, applied to a stateless server:

| Tool | What it does |
| --- | --- |
| `roll_find_tools` | Searches the tools this connection may use by what the agent wants to do (`query`, optional `product`, `limit` up to 25) and answers each match's name, description, input schema and which run tool takes it. With no query, it answers every tool's name and title. Words count by how rare they are among the tools, more in a name than a title than a description, and of two names holding the query, the one it covers more of comes first (`apps_deploy` before `apps_cancel_deploy`); a title that holds every word of the query counts double ("run a function now" is `tasks_invoke`, not `tasks_cancel_function_runs`). Question words (why, are, does) and plurals (`queries` is `query`) don't change a match. `search::finds` holds 73 everyday queries, across every product, to their tool. |
| `roll_run_read` | Runs a read-only tool: `{"tool": …, "arguments": {…}}`. It refuses one that changes things. |
| `roll_run` | Runs any tool the connection has. |

There are two run tools because clients decide what to run without asking from `readOnlyHint` and
`destructiveHint`. A single generic call tool would hide them; `roll_run_read` is read-only and
`roll_run` is destructive. A run tool appears only when there is something for it to run: a reader
or a read-only connection has no `roll_run`.

A run is the tool it names from the first check on: it reaches only a tool `tools/list` would
have listed, then the read-only check, the gate, approvals (`approval` goes in `arguments`) and
the audit log see that tool as if the client had called it by name. Search mode only changes what
`tools/list` answers. Toolsets and read-only narrow what it searches. Prompts and the instructions
stay the same, and the instructions say to run named tools through the run tools. Any value other
than `search` or `all` is a 400. `GET /v1/mcp/toolsets` reports search mode's cost (`search`), and
the connect panel has a checkbox for it.

Clients that can't take the whole list get search mode to start with: the connect panel turns it
on when the picked client's limit is passed (VS Code, Cursor), and `roll mcp config vscode` or
`cursor` writes `?tools=search` (`--all-tools` for the whole list; `--toolsets`, `--readonly` and
`--lockdown` set the rest of the mode; `--toolsets apps.observe,postgres` takes products and areas, and a product roll doesn't have is refused before a config is written). The server itself doesn't switch on `clientInfo`: the
2026-07-28 revision makes it optional and asks servers not to change behaviour on it. A
`claude mcp add` line quotes a URL with a mode, so its `&` doesn't end the command.

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

### Reaching outside roll

A tool that has roll reach an address the caller chose says so with MCP's `openWorldHint: true`
(`Tool::open_world`): publishing a task to a URL, a topic's endpoints, schedules and retries that
deliver, running the app's own functions, adding or testing a notification or a log drain, a
service from a Git URL or an image and its deploys, a sandbox (a public image, the internet by
default) and the commands run in it, and a managed secret's rotation at its vendor.
Every other tool's domain is roll's own (`false`). Search mode's `roll_run` is open-world when any
tool it may run is; `roll_run_read` never is.

### Output schemas

A 2026-07-28 client may run code against tools itself (Claude Code's code mode), so `tools/list`
gives it each tool's `outputSchema`: the fields its answer has. roll's routes have no response
schemas, so these are learned from real answers: `pnpm test:mcp-sweep --record` keeps what every
tool answered on a fresh rolld, and `scripts/mcp_output_schemas.py` widens
`crates/roll-mcp/src/output-schemas.json` with it. They're loose on purpose, so a client that
validates answers never trips on a product's next field: every value may be null, more fields are
always allowed, nothing is required, and nothing past four levels is described. A tool the sweep
hasn't seen answer has none. The sweep (and the nightly run on main) fails on an answer whose type
no longer fits the schema its tool publishes; rolld's `output_schemas_name_listed_tools` fails on
a schema for a tool that's gone. Older clients and search mode don't get them (`tools/list` stays
the size it was).

### Events

A 2026-07-28 client can ask what happened instead of being asked (MCP Events, SEP-3415,
`io.modelcontextprotocol/events`, offered in `server/discover`): an agent watching a deploy polls
`apps.event` and hears that it failed, a crash loop or an alert. Poll mode only (`events/list`,
`events/poll`); `events/stream` and webhook subscriptions aren't offered yet. An event type is a
feed route read through the gated router with the caller's token, as a tool is, and it's the tool
for that route's to list and allow (`apps.event` is `apps_events`'s: `GET /v1/apps/events`): a
connection that can't use the tool doesn't see the type and is refused (`-32024`), the gate
decides and audits each poll (as `events/poll:apps.event`), and lockdown withholds what the tool's
answer would. roll keeps nothing per client: the cursor (`a1:<seq>`) is the feed's own position.
No cursor starts from now (`?latest=1`: no events, the newest cursor); one roll can't read answers
a fresh cursor and `truncated: true`, never an error, as the SEP says. `arguments` filter by
`kinds` (`deploy.failed`, or a family as `alert.*`), `service` and `environment`; `maxEvents` (at
most 100) and `maxAgeMs` are honoured, events older than `maxAgeMs` skipped and marked `truncated`.
A full page (`hasMore`) means poll again at once; otherwise after `nextPollMs` (15 s).

### Credentials kept out of what agents write

As GitHub's MCP server's secret protection does (push protection for agents), a tool call that would
write a credential where people and other agents read it is refused before it's sent
(`roll_mcp::guard`): roll code (files, work items, comments, reviews), previews (notes), the
workspace (names, descriptions) and agents (messages). That's how a prompt-injected agent leaks a
secret: by writing it somewhere it's read.

- Where: also what a tool publishes (`Tool::published`: a status page's incidents and their updates, read by anyone).
- Found: AWS access keys (not AWS's documentation `…EXAMPLE` keys), GitHub tokens (`ghp_`…, `github_pat_`), Slack tokens, Stripe live keys,
  Google API keys, Anthropic and OpenAI keys, PEM private keys (a block with a base64 body, not
  a bare header in code that parses keys), database URLs carrying their password (Postgres,
  MySQL, MariaDB, Redis, MongoDB, AMQP, SQL Server and ClickHouse URLs whose password is 12 or more
  characters of two kinds or more, not a placeholder such as `${DB_PASSWORD}`, `<password>` or
  `password`, and not on loopback: a developer's `postgres:postgres@localhost` passes), a roll
  secret's share link (`shr_…#` and its key: the id alone is listed to readers), and roll's own credentials (`rlt_`,
  `rlo_`, `rlf_`, `rlc_`, `rlk_`, `rlp_`, `rlb_`, `rlq_`, by their checksum, so never a false positive). roll's are refused
  in every product but roll secrets, where keeping one is the point; the others only in the
  products above (a database, a queue or apps' variables are where credentials belong).
- The values of the project's own roll secrets too, for a token that may reveal them (the one that
  could have read a value to leak): the words a write would put where others read that look like
  credentials (as roll code's landings judge them: at least 12 characters, a digit or a capital,
  enough entropy, so `production` or `us-east-1` never match; all of them, and a write with more
  than 20,000, roll secrets' limit for a scan, isn't sent) go to roll secrets' own scan (`POST /v1/secrets/projects/{p}/leaks`)
  as the token, through the gate, for the projects its secrets rules name (or, with a rule over
  every project, the ones it can list, quietly: never asking a person; past 50 the write isn't
  sent). A scan that fails is logged, not silent. That scan only ever matches values the caller
  may reveal, so the guard is never a test of a guess. The refusal names the key and environment.
  The scan carries `X-Roll-Guard: 1` (roll secrets audits it as `secrets.guard`). A key tagged
  `sensitive` is matched only for runtime callers, never for a token (that would be a one-bit
  oracle on a value the token may never reveal), so the guard doesn't stop one; roll code's own
  push scan still catches it landing in a repo.
- Reads are never refused. The refusal says where in the arguments and what kind
  (`{refused: "credential", at: "arguments.body", kind: "a GitHub token"}`), never the value, and
  points at roll secrets. The audit log keeps a `deny` row with the same, without the arguments.
- Checked live against a real rolld and roll secrets (`live/guard-live.sh`): a GitHub token in a
  project's description refused and never echoed; a production secret's value refused for a token
  that may reveal it, named by key, through roll secrets' own scan and the gate; the same value
  through for a token that can't reveal it; a plain description through.
- One detector (`roll_auth::credentials`, beside the checksum of roll's own secrets) serves both
  MCP servers: roll code's own `/v1/code/mcp` refuses the same formats in its writes (`land`,
  changes, work items) before they're sent.
- Nothing reached its destination, so nothing is revoked (a credential that did leak, in a landing
  or a report from GitHub, still is: `Access::revoke_leaked`).

### The sweep

`pnpm test:mcp-sweep` (`scripts/test-mcp-sweep.sh`) builds rolld and starts it with every product
on, its listeners on loopback port 0 (the addresses read back from `--addr-file`, #596) and a
throwaway home. It then calls every tool through
`/v1/mcp` as an agent would (`scripts/mcp-sweep.py`):

1. It makes fixtures with the write tools.
2. It calls each read-only tool with ids from them. Where no fixture makes the thing a tool names
   (a deploy, a trace, a mail), it sends a stand-in that doesn't exist: the route answers "not
   found" with a message, so the tool's path, query and types are still checked (`--verbose`
   lists each stand-in and its answer).
3. As a member: it signs a person in by email code (rolld's outbox), makes them a viewer in its
   project, mints them a token that names every action, and calls each read tool of a member
   product (secrets, previews, workspace) their client lists. None may be refused by their role,
   and secrets' writes and reveals must not be listed for them.
4. It calls each write tool with values made from its schema, the destructive ones last.

A 5xx, an argument the route rejects (unknown, missing, or of a type it can't parse), or an empty 404
(no route answers the path) fails it. That's what the unit tests can't see:
`tools_map_onto_classified_routes` checks a tool against the catalogue, not against the product's
own handler. It found that an id's `_` sent as `%5F` left a service made over MCP never owned by
apps' routing (#402), and that two secrets tools sent their numeric seq and version as any string.
Every read is also called as a token that may read everything and reveal nothing: a credential the
fixtures made, in its answer, fails it. Tools that need a running Postgres endpoint aren't reached
without Docker. It skips itself when nothing changed (`--force`; `--reads` skips the writes).

### Lockdown

`/v1/mcp?lockdown=1` (or `X-MCP-Lockdown: true`) works like GitHub's MCP server's lockdown mode:
text written by someone who can't push to the repo never reaches the model. roll code's own server
takes the same switch (`/v1/code/mcp?lockdown=1`, docs/code/mcp.md); both use `roll_auth::lockdown`.

The risk it covers: anyone can open an issue on a public GitHub repo, and roll code makes the
issue a work item that an agent claims and reads. An instruction hidden in the issue would be read
by a model that holds write tools.

How it works:
- Products mark records from outside `"outside": true`:
  - roll code's work items from GitHub issues whose author isn't `OWNER`, `MEMBER` or
    `COLLABORATOR` (a missing association counts as outside);
  - its changes from forks (`!trusted`);
  - reviews and review comments whose author can't push: from GitHub by the same
    association, and from roll's own API when the caller may comment but not write (a public
    repo's reader).
- In lockdown, every result has the `title`, `body`, `description`, `message` and `text` of those
  records replaced with a notice, wherever they sit in the answer.
- A tool whose answer anyone outside can write names those fields itself (`Tool::outside`):
  `apps_form_submissions` withholds every submission's `fields`, since anyone can submit a site's
  form, and so do `apps_events` and `apps_activity` (a `form.submitted` event names the fields the
  visitor posted); `search_analytics` withholds each `query`, what a cluster's users typed into a search
  box. Ids, times and counts stay. Logs, errors and traces aren't withheld, though visitors' requests
  shape them too (paths, user agents, messages): without them nothing could be debugged, and the guide
  tells agents that what outsiders wrote is data, never instructions.
- A tool's text block is then rebuilt from what's left, because a tool set may have written its
  own text block.
- Ids and states stay, so the agent can still say what's there. The instructions tell it not to
  act on what it can't read.
- The flag parses like `?readonly=`: a typo is a 400, never "off".
- The connect panel has a checkbox for it.

Chat messages that reach an agent aren't marked yet (`## Next`).

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
// Cursor (~/.cursor/mcp.json); VS Code since 1.146 (~/.copilot/mcp-config.json, or .mcp.json at a
// workspace's root, never with a token in it) with "type": "http", its old .vscode/mcp.json going in
// February 2027; Devin's desktop app, where Windsurf's docs now point (~/.config/devin/mcp_config.json),
// takes the same with "transport": "http"
{ "mcpServers": { "roll": { "url": "https://api.roll.sh/v1/mcp" } } }
```

Claude.ai: Settings → Connectors → Add custom connector → `https://api.roll.sh/v1/mcp`.

One click for the clients that take a link: the connect panel's "Add to Cursor"
(`cursor://anysphere.cursor-deeplink/mcp/install?name=roll&config=…`) and "Install in VS Code"
(`vscode:mcp/install?…`) carry the server's URL with the panel's choices, never a token: a link
lands in the browser's history, and with OAuth the person chooses what the client may do when it
signs in.

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
person's reason). A 2026-07-28 client that declares URL elicitation in its request's
`clientCapabilities` gets it as a Multi Round-Trip Request instead: the result is
`{resultType: "input_required", inputRequests: {approval: {method: "elicitation/create", params:
{mode: "url", url, message}}}}`, and its retry (with `inputResponses`) runs once a person approved.
No `requestState` is needed: the gate matches the retry to its approval itself, so nothing the
client echoes can decide what runs.

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
- A client that names roll's MCP server as its `resource` (RFC 8707; MCP clients always do) gets
  tokens for that server only: they work at `/v1/mcp` and in the calls the server makes for them,
  and the API refuses them directly (`401 invalid_token`), so what the server adds (lockdown, the
  credential guard, a read-only URL) can't be stepped around with its token. Refreshing keeps the
  audience; a token request naming another resource is `invalid_target`. A client that names none
  (`roll login`) gets API tokens.
- Codes are good for one minute, once. Access tokens (`rlo_`) last an hour. Refresh tokens (`rlf_`)
  rotate on every use, and a used one fails; a connection expires 30 days after its last refresh.
- `scope` (`read-only`, `agent`, `project`, `development`, `previews`, `full`) picks the preset the
  consent screen starts from. The user decides what's granted, and the consent screen is the only
  place a connection's rules come from: a client can't widen them.
- Each connection is a grant: it's listed with its client, last use and rules, and revoking it ends
  every token it has, at once.
- Step-up (SEP-835): when an OAuth connection's own rules refuse a tool call and adding one rule
  would let it through (not a refusal by the person's role in a project, a move between projects, a
  copy or a link, or a rule over tokens and approvals), roll records a step-up: that connection and the
  one rule the call needed, exactly as the gate decided it (the resource and an index included),
  kept 10 minutes, at most 10 a connection. The refusal carries
  `Bearer error="insufficient_scope", scope="roll:step-up:<id>", resource_metadata=…` in
  `_meta["mcp/www_authenticate"]` (ChatGPT reads it there) beside its text and widen link, and a
  single, unbatched call from a client known to step up (Claude Code from 2.1.288) answers HTTP
  `403` with that `WWW-Authenticate` header. The consent screen looks the id up as the signed-in
  person (`GET /v1/access/oauth/step-ups/{id}`: theirs, live, or one "no such step-up" for any
  other case), shows the rule and the connection it joins (said plainly when it reaches far, or
  when the policy has an admin approve it), and "Add it" sends only the id: the rule joins that
  connection's stored rules in one transaction that rechecks it (live, not waiting on an admin) and
  meets the token policy, used once, nothing else about the connection changes, and the client's code is for that connection (its other sessions widen too, nothing new
  is made). A client never chooses what's proposed. A scoped token (`rlt_`) has no consent to step
  up, and a progress-streamed call keeps the challenge in-band only.
- Clients either register (dynamic registration) or are known by a client ID metadata document
  (2025-11-25: the `client_id` is an https URL whose JSON names it). A document is fetched through
  roll's private-address guard (`roll-net`), without redirects, in 5 seconds, at most 64 KiB, and
  at most once an hour; it must name itself, list valid redirect URIs, and be a public client
  (checked against draft -03: no `client_secret`, `none` among its methods, codes among its grants;
  an id with a user or password in it, or not in normal form, is refused, since the URL fetched is
  the one compared, and so is roll's own address). A document unreachable for a moment (a timeout,
  a 5xx) leaves roll's copy in use; one that's become invalid isn't. A loopback redirect matches on
  any port (RFC 8252), for registered clients too: a native app listens where it can.
- roll's policy may list the clients that connect (`oauth_clients`; Atlassian's admins allowlist AI
  clients): a host (`claude.ai`, `*.example.com`), an exact `https://` URL, `localhost`, an app's
  scheme (`cursor:`) or `device`. A metadata-document client matches by its URL or host; a
  registered one only by where every one of its redirects goes (its name is anyone's to choose,
  its redirects are where its codes go); a device client only by `device`. A client the list leaves
  out isn't offered consent (the screen says why), can't start a device sign-in, trade a code or a
  refresh token (refused before either is spent), or use the connection it has (`401
  invalid_token` saying so): nothing is revoked, so allowing it again brings the connection back.
  Machine clients (client credentials, workload identity, ID-JAG) are an admin's own setup and
  aren't held to it.

**A person's own connection.** A token or OAuth client of a person who isn't an admin reaches
what their account does (`roll_access::USER_REACH`, held to rolld's gate by its `auth::tests`):
roll code and agents through their own owners, and the projects they're members of, with those
projects' secrets and previews as their role allows (docs/members.md). `tools/list` shows only
those products' tools, and on one resource of a member product only what their role allows too,
as the gate decides each call (`within` their session): a viewer's client lists secrets' reads,
not its reveals or writes, whatever the token names. A list or a create goes on the token's rules
alone, as at the gate.

## Protocol

- Streamable HTTP, JSON-RPC 2.0, protocol versions `2026-07-28`, `2025-11-25`, `2025-06-18`,
  `2025-03-26` and `2024-11-05`: a dual-era server, as 2026-07-28's compatibility matrix puts it.
- **2026-07-28, the stateless core** (`roll_mcp::modern`): a request whose `_meta` names
  `io.modelcontextprotocol/protocolVersion` is served on its own, with no `initialize` and no
  session; what the client offers (`clientCapabilities`, `clientInfo`, for the audit log) comes
  with each request.
  - Its headers must say what its body says: `MCP-Protocol-Version`, `Mcp-Method`, and `Mcp-Name`
    for `tools/call`, `resources/read` and `prompts/get` (the `=?base64?…?=` form decoded), or 400
    `HeaderMismatch` (`-32020`), so a proxy routing on headers and roll running the body agree. A
    version roll doesn't speak is 400 `UnsupportedProtocolVersion` (`-32022`) with `supported`; an
    unknown method is 404 with `-32601`. It's one message, never a batch. A resource that isn't
    there is `-32602` (Invalid Params, SEP-2164); the handshake revisions keep their `-32002`.
  - `server/discover` answers the versions, capabilities and instructions, and its `serverInfo`
    has roll's icon (the Horizon mark, as `initialize`'s does); other answers name the server
    without it. Every result has
    `resultType` and `_meta["io.modelcontextprotocol/serverInfo"]`; lists (and discover) have
    `ttlMs: 60000` and `cacheScope: "private"`: a token's tools are its own, and its rules can
    change.
  - Approvals are Multi Round-Trip Requests (see Approvals).
  - `subscriptions/listen` (SEP-2575) is an open stream: first
    `notifications/subscriptions/acknowledged` with what roll will send of what was asked
    (`toolsListChanged`, `promptsListChanged`), then `notifications/tools/list_changed` (and the
    prompts one) whenever the connection's tools change: an access request approved, a rule
    remembered, the token edited. Each carries `_meta["io.modelcontextprotocol/subscriptionId"]`.
    The grant is looked up again every 5 s, never cached; when it stops working, or after an hour,
    the stream answers the listen request (`result: {}`, the graceful end the spec added after
    SEP-2575) and closes, and the client listens again. At most 4 are open at once per token; a
    listen asking for nothing roll sends is refused (`-32602`). A 2026-07-28 client hears list
    changes only there; an older one hears `list_changed` on the call that approved an access
    request.
    Checked live with the official TypeScript SDK v2 (`live/sdk2/listen.mjs`): its `listChanged`
    option opens the subscription by itself, and an access request approved mid-session refreshes
    its tools (9 to 18). That run found two interop bugs, both fixed: the ack must carry the
    subscription id too (the SDK matches the ack by it), and the SDK sends `notifications/cancelled`
    without `Mcp-Method` (a notification may leave it out; one present must still match).
  - **Asking for a missing id** (`roll_mcp::ask`): a call without an id it requires, from a
    client that declared form elicitation, answers `input_required` with a form whose choices are
    what the connection may read (the list tool's answer, through the gate, never asking a
    person; completions use the same ones). Every id a tool lists has choices: a workspace or
    Postgres project, a branch, an apps service or environment, a secrets environment, a preview,
    a token, a bucket, a stream or topic, a NoSQL or Redis database, a search cluster or index, a
    vhost or queue, a tasks message, run, schedule, function, event or dead letter (tasks' `id`, by
    the tool's route), and for roll code's tools (fronted from `code_control::mcp`) a repo as
    `owner/name` or its owner. `pnpm test:mcp-sweep` checks each against a live rolld: the form must offer
    the fixture's id, so a list answer of another shape can't offer nothing unnoticed, and a new
    kind of id fails it until it has choices or a reason not to (`NO_CHOICES`: a Postgres
    endpoint, which sits inside its branch, and no tool lists every project's). The
    retry's `inputResponses` fills the id in, and the call runs as if the client had sent it, so
    the gate decides it as usual and there's no `requestState` to trust. A declined form stops the
    call; a value that isn't an id is refused. Other clients get the call's own answer. Checked
    live with the official TypeScript SDK v2 (`live/sdk2/ask.mjs`: its elicitation handler picks
    from the form, and it retries by itself).
  - **MCP Apps** (`roll_mcp::ui`, `io.modelcontextprotocol/ui`, SEP-1865): for a host that
    declares HTML views (`mimeTypes: ["text/html;profile=mcp-app"]`), the read-only tools that
    answer rows (lists, queries, logs, reads, usage) point at `ui://roll/table` (`_meta.ui`). The
    host renders that view in a sandboxed iframe, and it shows the tool's result as a table, light
    or dark as the host is: times as UTC, columns empty in every row dropped, and links roll
    answered (an approval's page, a preview) opened by the host (`ui/open-link`) where it can.
    `access_wait_approval` points at it too: while a call waits for a person, the view is a card
    with roll's approval page to open. A deploy or a job (`apps_get_deploy`, `apps_get_job` and their
    `apps_wait_*` twins) shows too: an object's own fields (status, commit) above its list (the
    steps).
    - **Declaring it:** a 2026-07-28 request on itself; in `initialize`, the session id carries it
      (`~ui.` after its prefix), as URL elicitation does.
    - **Safety:** the view has no network (its resource declares no domains), writes every value
      as text, and calls no tools. It only asks the host to open an `http(s)` link roll answered.
      An approval is never decided from a view, because an agent's token could drive it.
    - **Checked:** `a_host_that_renders_apps_gets_the_table_view` (roll-mcp); the table in
      ext-apps' reference host (`examples/basic-host` v2.0.4, `users-live/host-shot.mjs`); the
      table and the approval card in a stand-in host that checks the handshake as the SDK's
      AppBridge does (`users-live/app-shot.mjs`).
  - **Tasks** (`roll_mcp::tasks`, the `io.modelcontextprotocol/tasks` extension, SEP-2663):
    - **When:** a client that declares the extension on its request gets a task handle
      (`resultType: "task"`) for a call still running after 3 s, or one waiting for a person's
      approval. Everyone else waits for the answer, as before. `server/discover` lists the
      extension only where tasks can be made.
    - **Running:** the call goes on in the background on the node that made the task, as the same
      gated request with the caller's own headers, so the token is looked up again when it runs.
      One waiting for approval shows the approval page: `input_required` with a URL elicitation,
      or a status message. There's one input request per approval, keyed by the approval's id, so
      a client shows it once however often the task waits again. Once a person decides, it runs.
    - **Waiting tools:** a tool that waits for something up to a request's limit (`tasks_wait_run`'s
      run, `access_wait_approval`'s decision, `apps_wait_deploy`'s deploy, `apps_wait_job`'s job) waits on as a task. Its answer is checked (a tool's
      `until`), and while it says the wait isn't over the task asks again every 2 s, so it waits
      as long as it takes. Such a call becomes a task as soon as its first answer isn't final.
    - **Polling:** `tasks/get` answers from any node, with the result inline once `completed` (a
      tool's `isError` is still `completed`) or the JSON-RPC error once `failed`. `tasks/update`
      is acknowledged: the approval itself is decided on roll's page. `tasks/cancel` stops it.
    - **Who sees it:** a task is its owner's (the grant, or the operator), looked up on every
      request. Anyone else's poll, or an unknown or expired id, is `-32602`. A request that didn't
      declare the extension is `-32021`. `Mcp-Name` carries the task id.
    - **Storage:** the id is the caller's secret (only its hash is stored). Results are sealed at
      rest, as audit sinks' secrets are (a reveal's answer is a credential), so tasks need
      `ROLL_SECRET_KEY`.
    - **Limits:** a task lives an hour, a token runs at most 16 at once, and a working task its
      node stopped touching for a minute reads as `failed`. Results are cut past 4 MiB.
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
- **Server card** (SEP-2127, the experimental `server-card` extension; `roll_mcp::card`): what a
  registry, a crawler or a client learns before connecting, public and the same for everyone.
  - `GET /v1/mcp/server-card` (`application/mcp-server-card+json`, the location MCP reserves)
    holds roll's name (`sh.roll/roll`), version, title, description, icon and website, and one
    `streamable-http` remote: `/v1/mcp`, an optional `Authorization: Bearer` header (without one,
    OAuth), and the versions `server/discover` answers.
  - `GET /.well-known/ai-catalog.json` (`application/ai-catalog+json`) is the domain's AI Catalog,
    with one entry pointing at the card.
  - It never lists tools or anything a token decides, and clients may not trust it for access
    decisions (the spec's rule). It validates against the extension's `schema.json`.
- Requests with an `Origin` header must come from the dashboard's or the API's origin (DNS
  rebinding).
- Results come back as `structuredContent` (the route's JSON; arrays as `{"items": […]}`) and a
  `text` block, cut at 48 KiB (inline in Claude Code, which moves an answer past 50,000 characters or
  its default 25,000 tokens to a file, even for dense JSON), saying so and that the tool can be asked for less. Binary content comes back base64 (`content_b64`). An image (a preview's screenshot: PNG, JPEG or WebP, up to
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

Two templates (`resources/templates/list`). Listed when the connection may list workspace projects:
`roll://projects/{project}` is a project and every resource on its canvas (kind and id, names
quoted), read from `GET /v1/workspace/projects/{project}` the same way, for a client to attach
before acting on a project. Its id completes like a prompt's (`completion/complete` with
`ref/resource`); an id that isn't letters, digits, `_` and `-` is "resource not found", so a URI
never reaches another route.

Listed when the connection has `code_read_file`: `roll://code/{owner}/{repo}/{+path}` is a file on a
roll code repo's default branch, as GitHub's server has repository contents as resources, for a
client to attach a file as context. It's read through `code_read_file` as the caller (the gate
decides, the audit log has the call); a binary file says what it is, and a long one says where
`code_read_file` reads on. Owner and repo are ids and complete from `code_list_repos`; a path's
segments are percent-decoded and never empty, `.`, `..` or a hidden `/`, else "resource not found".

## Prompts

roll's workflows as prompts a client offers as slash commands (`prompts/list`, `prompts/get`;
`crates/roll-mcp/src/prompts.rs`), as GitHub's and Sentry's MCP servers ship theirs. A prompt is
only text that walks the agent through roll's tools: it calls nothing, each tool it names is still
decided by the gate, and a connection is offered only the prompts whose tools it has. Arguments are
ids (letters, digits, `_`, `.`, `-`, at most 64), quoted in the message: anyone can name a project,
and a prompt's text is the person's own message to the agent.

A prompt's arguments complete as the person types (`completion/complete`): the values come from
the list tool the connection would call (`workspace_list_projects`; a Postgres project's from
`postgres_list_projects`; `apps_list_services`, `apps_list_environments` for the `service` already
given; `previews_list`; `queues_list_vhosts`, `queues_list_queues`; `streams_list`,
`streams_list_groups` for the `stream` already given), through the gate
with the caller's token, so a suggestion is only what it may read, and each is an audited read.
Only ids are offered (what tools take). A completion never asks a person: a list that would ask
is refused at once (`would_ask`), so typing opens no approval. A prompt the connection isn't offered
is `-32602`.

| Prompt | Arguments | What it does | Tools |
| --- | --- | --- | --- |
| `what_needs_me` | | Across roll, what's waiting on you now in three groups (act now, soon, worth knowing): pending approvals, environments' attention and recent events, exposed or leaked secrets, failed previews and forks waiting for `approve-run`, dead letters, risky tokens, broken-main items; each with the call that shows it. Offered to every connection; it uses the tools the connection has. Changes nothing. | the connection's own |
| `what_can_i_do` | | What this connection may do, in words, and which rule would allow what's missing. | `access_whoami` |
| `check_diff_for_secrets` | `project` | Before a commit: whether any added value is one of the project's stored secrets (by key and environment, never the value). | `secrets_scan` |
| `respond_to_leaked_secret` | `project`, `key` (optional) | A secret that may have leaked: whether it's live (`secrets_credential_check`), who and what reaches it and who read it lately (`secrets_reach`), a plan to rotate it and redeploy what reads it, done only once you say, then a check that it's no longer exposed. Never a value. | `secrets_health`, `secrets_reach`, `secrets_rotate` (and `secrets_credential_check`, `secrets_set`, `apps_redeploy`, `secrets_list_rotations` where the connection has them) |
| `fix_secrets_health` | `project` | What needs fixing in a project's secrets, most urgent first, asking before changing anything. | `secrets_health` |
| `try_migration_on_branch` | `project` | A schema change tried on a branch first (create, migrate, compare), then on main only when it worked and you say so. | `postgres_get_project`, `postgres_create_branch`, `postgres_describe_schema`, `postgres_execute`, `postgres_delete_branch` |
| `compare_branch_schemas` | `project`, `branch` | What a branch changed in the schema against main (Neon's schema compare), and the advisors' findings there; changes nothing. | `postgres_get_project`, `postgres_describe_schema`, `postgres_advisors` |
| `tune_slow_query` | `project` | A slow query made faster on a branch first (Neon's query tuning): its plan, an index tried on a copy, both plans compared, main only when you say so. | `postgres_get_project`, `postgres_slow_queries`, `postgres_explain`, `postgres_create_branch`, `postgres_execute`, `postgres_delete_branch` |
| `security_checkup` | | Which tokens need attention, most serious first: a narrower rule set for each over-broad one, where roaming ones were used from, which to revoke or give an expiry; adds secrets' findings, a project's secrets audit log that doesn't hold (`secrets_verify_audit`), unmasked personal data, over-broad or unused S3 keys, public buckets, search clusters with no IP allowlist, previews shared outside the team and live images with high or critical vulnerabilities when the connection has those tools. Changes nothing. | `access_checkup`, `access_tighten_token`, `access_token_networks` |
| `debug_failing_service` | `service`, `env` | Why an environment is failing (what roll already flags in its `attention`, the whole timeline, deploys, deploy logs, failing requests and routes, metrics), then a fix or a rollback it asks about first. | `apps_get_environment`, `apps_activity`, `apps_route_stats`, `apps_list_deploys`, `apps_deploy_logs`, `apps_logs`, `apps_metrics` |
| `ship_a_service` | `project` | A new app service from a repo, a Git URL or an image: what its project file would make (`apps_import`), the service with its links and variables, a deploy outside production first, checked (environment, logs, errors), and production only when you say. | `apps_create_service`, `apps_deploy`, `apps_wait_deploy`, `apps_get_environment`, `apps_logs` |
| `tune_slow_search` | `cluster` | Why a search cluster answers slowly: its slowest requests, a profile of the worst, the usual causes in mapping and query, and a fix tried on a branch (`search_branch_cluster`) before the cluster. | `search_slowlog`, `search_cluster_metrics`, `search_get_mapping`, `search_query` |
| `review_preview` | `preview` | Whether a change's preview is ready to land: why it failed or waits for `approve-run`, its schema changes (a dropped column, a `NOT NULL` with no default), new errors, routes slower than main's preview, its pages against the base (screenshots it looks at), people's notes. Changes nothing without asking. | `previews_wait`, `previews_get`, `previews_logs`, `previews_errors`, `previews_routes`, `previews_screenshot`, `previews_list_notes` |
| `hand_to_agent` | `owner`, `repo` | A problem (an error group, a slow query, a bug described) as a work item with its evidence quoted as data, handed to a coding agent on one of your machines (a thread in its own worktree, `auto_edit`, the prompt shown to you first, the item bound to it), its requests left to you, its branch published as a change, checked and previewed, and queued only when you say. | roll code's and agents' (`code_create_work`, `agents_create_thread`, `agents_claim_work`, `agents_publish_thread`, `code_get_change`, `code_enqueue_change`, …; rolld's tests hold each name to one roll code fronts) |
| `recover_deleted_objects` | `bucket` | Objects deleted by mistake: what's still in the bucket's trash, narrowed to what you describe, each restored only once you say and checked after. | `storage_list_trash`, `storage_restore_object`, `storage_head_object` |
| `try_cql_change_on_branch` | `database` | A NoSQL schema or data change tried on a branch (as `try_migration_on_branch` for Postgres): applied there, checked with the branch's schema and `nosql_diff`, then on the database only when you say. | `nosql_schema`, `nosql_create_branch`, `nosql_cql`, `nosql_diff` |
| `find_redis_memory_hogs` | `database` | What fills a Redis database, reading only: totals (`INFO`, `MEMORY STATS`), a sample of keys by size, type and TTL grouped by prefix, keys over 1 MB, what has no expiry, and what to change. | `redis_usage`, `redis_command` |
| `clear_task_backlog` | | Why roll tasks' messages pile up or fail: a paused or limited queue, dead letters and each delivery's error grouped by cause, then a retry it asks about first (with how many). | `tasks_stats`, `tasks_list_queues`, `tasks_get_queue`, `tasks_list_dead_letters`, `tasks_get_message` |
| `drain_queue` | `vhost`, `queue` | Why a queue backs up: no consumers, consumers that don't acknowledge (and which connection), rates, and why its dead-letter queue's messages died; a replay it asks about first, never a purge. | `queues_get_queue`, `queues_queue_rates`, `queues_list_channels`, `queues_peek`, `queues_list_bindings` |
| `find_consumer_lag` | `stream`, `group` | Why a consumer group falls behind: lag per partition, members against partitions, throughput, and the record a stuck partition stops at; a reset (which skips records) it asks about first. | `streams_get_group`, `streams_get_topic`, `streams_usage`, `streams_read` |
| `review_costs` | `project` | Where a project's money goes this month (apps by service, projected; previews against their budget and rules; the canvas's databases, buckets, streams and clusters when the connection has their tools), against last month and its limits, with what would cost less for the top three. Changes nothing without asking. | `apps_project_usage`, `apps_limits`, `previews_usage`, `previews_settings` |

### As skills

The same workflows, and the guide, are Agent Skills too (`io.modelcontextprotocol/skills`,
SEP-2640; `crates/roll-mcp/src/skills.rs`), for hosts that load skills rather than offer slash
commands:

- **Declared:** `server/discover` (and `initialize`) list the extension as `{}`. roll serves no
  directories.
- **Listing and getting:** `skills/list` and `skills/get` answer each skill's entry: its frontmatter
  (`name`, `description`) and its one file with its size and `sha256` digest. An unknown or
  unoffered URI is `-32602`.
- **Reading:** `resources/read` of `skill://roll/<name>/SKILL.md` answers the file. `resources/list`
  lists them too.
- **What's served:** `using-roll` (the guide) and `set-up-ci` (a repository's CI signing in with
  its own identity: the one command the person runs, the workflow step the agent writes) for
  everyone, plus one skill per workflow the connection's tools allow, its arguments written as
  `<project>`. The listing is per connection
  (`cacheScope: "private"`).
- **Safety:** skills come only from roll's binary, never from tenant text. They name no
  `allowed-tools` and nothing to run, and the gate decides every tool call they lead to.
- **Checked by:** `skills_are_the_workflows_a_connection_has` (each file reads back with its listed
  size and digest, and its frontmatter matches the entry) and
  `skills::tests::every_skill_follows_the_agent_skills_rules`.

## Tools

Every catalogued route of a product here is a tool's, or a line in `crates/roll-mcp/src/no-tool.txt`
with why it has none: it answers or sets a credential, it's a person's call, the operator's, the
dashboard's, interactive, the data plane, an outside integration's setup, or `no tool yet` (a gap
to close). A test fails on a route with neither, and on a line whose route has a tool now, so a
product's new route can't go without a decision (as `prefix-reviewed.txt` does for the
catalogue). A line may name a route an open PR is about to add, so that PR lands green.

Arguments are checked against each tool's JSON Schema (closed: unknown arguments are refused).
Every tool also takes `approval` (see [Approvals](#approvals)). Creating tools take
`workspace_project`, the workspace project the new resource goes in; a token limited to projects
needs it. A `null` argument is left out, as if it weren't sent (some clients fill every field they
don't use with one). A tool with fields its route clears (an environment's `egress` or `routes`, a
domain's redirect, a queue's rate limit) takes `clear`, a list of those fields' names: only a field
named there is sent as `null`. Tools for products that are off aren't listed. Tools for search, secrets, apps and
previews are listed once those products are on.

| tool | what it does | route |
| --- | --- | --- |
| `access_whoami` | The connection's identity: the operator, or the token or OAuth connection and the rules it has. | `GET /v1/access` |
| `access_wait_approval` | Wait (up to 55 s) for a person to decide an approval this connection asked for; then call the original tool again with approval. | `GET /v1/access/approvals/{approval_id}` |
| `access_check` | Whether this connection may make a request (allow, ask or deny), and as which action on what, without making it. | `POST /v1/access/check` |
| `access_request_access` | Ask a person for one more rule for a while (15m, 1h, 8h or 1d), saying why; wait with `access_wait_approval`. Once it's approved, that call's answer says the tools changed: `notifications/tools/list_changed` first on its stream (for a client that accepts one; `tools.listChanged` is declared) and in its text. | `POST /v1/access/requests` |
| `access_list_approvals` | Approvals, newest first: pending ones wait for the operator. | `GET /v1/access/approvals` |
| `access_audit_log` | Every request made with a token and every MCP tool call, newest first. | `GET /v1/access/audit` |
| `access_checkup` | The live tokens that need attention (everything, never expires, unused, narrower, roams), most serious first, each with its fix. A person's are their own. | `GET /v1/access/checkup` |
| `access_list_tokens` | Tokens and OAuth connections you may see (rules, expiry, last use; never a secret). | `GET /v1/access/tokens` |
| `access_get_token` | One token. | `GET /v1/access/tokens/{id}` |
| `access_tighten_token` | A token's rules narrowed to what it used (a proposal; applying it is the operator's). | `GET /v1/access/tokens/{id}/tighten` |
| `access_token_networks` | The networks a token was used from, first and last seen. | `GET /v1/access/tokens/{id}/networks` |
| `workspace_list_projects` | Workspace projects, each with the resources on its canvas (kind, ref, position). | `GET /v1/workspace/projects` |
| `workspace_get_project` | A workspace project and its resources. | `GET /v1/workspace/projects/{id}` |
| `workspace_create_project` | A new workspace project to group resources. | `POST /v1/workspace/projects` |
| `workspace_update_project` | Rename a project, or change its description or icon (a monogram, an emoji or a small image, with a colour; `clear: ["icon"]` goes back to the default). | `PATCH /v1/workspace/projects/{id}` |
| `workspace_delete_project` | Delete a workspace project. Its resources stay. A project that holds secrets needs a secrets admin's confirmation in the dashboard. | `DELETE /v1/workspace/projects/{id}` |
| `workspace_list_roles` | The roles a project member can have, the rules each grants, the products they cover, and the environments a developer reaches when none are named. | `GET /v1/workspace/roles` |
| `workspace_place_resource` | Put a resource on a project's canvas (or move it), at x, y. | `PUT /v1/workspace/projects/{id}/resources/{kind}/{ref}` |
| `workspace_remove_resource` | Take a resource off a project's canvas (the resource itself stays). | `DELETE /v1/workspace/projects/{id}/resources/{kind}/{ref}` |
| `workspace_list_members` | Who is in a workspace project: handle, role (viewer, developer, admin, owner), a developer's environments, who added them and when. Emails only for the project's admins. | `GET /v1/workspace/projects/{id}/members` |
| `workspace_invite_member` | Invite an email address to a project with a role (never more than the caller's own). The answer has the invite link once; the person accepts it by signing in as that address. | `POST /v1/workspace/projects/{id}/invites` |
| `workspace_set_member_role` | Change a member's role or a developer's environments. Both the old and the new role must be within the caller's own; the last owner can't be demoted. | `PATCH /v1/workspace/projects/{id}/members/{user}` |
| `workspace_remove_member` | Take a person out of a project: their session and tokens stop reaching it at once, and their tokens confined to it are revoked. The last owner can't be removed. | `DELETE /v1/workspace/projects/{id}/members/{user}` |
| `postgres_list_projects` | Postgres projects with their branches and endpoints. | `GET /v1/projects` |
| `postgres_get_project` | A Postgres project with its branches and endpoints (connection strings need reveal). | `GET /v1/projects/{project}` |
| `postgres_create_project` | A new serverless Postgres database (a project with a main branch and endpoint). | `POST /v1/projects` |
| `postgres_delete_project` | Delete a project with all its branches and data. Irreversible. | `DELETE /v1/projects/{project}` |
| `postgres_create_branch` | A copy-on-write branch of a project, from its parent now or at a point in time. | `POST /v1/projects/{project}/branches` |
| `postgres_delete_branch` | Delete a branch and its compute. | `DELETE /v1/projects/{project}/branches/{branch}` |
| `postgres_branch_history` | The range a branch can be restored to or branched from. | `GET /v1/projects/{project}/branches/{branch}/history` |
| `postgres_create_replica` | A read-only endpoint on a branch that follows its primary. | `POST /v1/projects/{project}/branches/{branch}/replicas` |
| `postgres_restore_branch` | Rewind a branch in place to a past point (the current state is kept as a branch). | `POST /v1/projects/{project}/branches/{branch}/restore` |
| `postgres_reset_branch` | A branch back to its parent's current state, in place (Neon's reset from parent). | `POST /v1/projects/{id}/branches/{branch}/reset` |
| `postgres_get_endpoint` | An endpoint's compute state and limits. | `GET /v1/endpoints/{id}` |
| `postgres_update_endpoint` | An endpoint's autoscaling limits and suspend timeout. | `PATCH /v1/endpoints/{id}` |
| `postgres_query` | Run read-only SQL on an endpoint: it runs in a read-only transaction, so writes fail. Returns columns and rows. | `POST /v1/endpoints/{endpoint}/query` |
| `postgres_execute` | Run SQL that may change data or schema (INSERT, UPDATE, DDL) on an endpoint; without `admin` on the endpoint, as a role that owns the database's objects, never the superuser (decision 6). | `POST /v1/endpoints/{endpoint}/query` |
| `postgres_describe_schema` | Every table's columns and types on an endpoint (read-only). | `POST /v1/endpoints/{endpoint}/query` |
| `postgres_explain` | A statement's plan as JSON, without running it (Neon's explain). | `POST /v1/endpoints/{id}/query` (read-only, `EXPLAIN (FORMAT JSON)`) |
| `postgres_activity` | Every client session now, blocked ones first: user, app (`roll tok_…`: SQL a token sent through roll), state, what it waits on, `blocked_by`, how long its transaction and query have run, the query. | `POST /v1/endpoints/{endpoint}/query` (fixed, read-only) |
| `postgres_health_checks` | A database's health in one read (Neon's `inspect_database`): cache and index hit rates, dead-row bloat, the oldest open transaction, idle-in-transaction sessions, connections against the limit, transaction ID age, size; each ok or warn. | `POST /v1/endpoints/{endpoint}/query` (fixed, read-only) |
| `postgres_slow_queries` | The 20 slowest statements by mean time (Neon's slow queries; `pg_stat_statements`). | `POST /v1/endpoints/{id}/query` (read-only) |
| `postgres_advisors` | Unused indexes, unindexed foreign keys, tables without a primary key, sequential scans (Supabase's advisors). | `POST /v1/endpoints/{id}/query` (read-only) |
| `postgres_list_extensions` | Extensions installed and available. | `POST /v1/endpoints/{id}/query` (read-only) |
| `postgres_typescript_types` | TypeScript interfaces for every table (Supabase's generated types). | `POST /v1/endpoints/{id}/query` (read-only) |
| `postgres_start_endpoint` | Wake an endpoint's compute. | `POST /v1/endpoints/{endpoint}/start` |
| `postgres_suspend_endpoint` | Suspend an endpoint's compute. | `POST /v1/endpoints/{endpoint}/suspend` |
| `postgres_update_project` | How long a project keeps history for point-in-time restores and branches. | `PATCH /v1/projects/{project}` |
| `postgres_backup_branch` | Take a base backup of a branch now (restores and new branches start from the newest). | `POST /v1/projects/{project}/branches/{branch}/backup` |
| `postgres_delete_endpoint` | Delete an endpoint (a read replica, or a branch's compute; its data stays on the branch). | `DELETE /v1/endpoints/{endpoint}` |
| `postgres_usage` | Compute and storage use of a project, with cost estimates. | `GET /v1/projects/{project}/usage` |
| `redis_list_databases` | Serverless Redis databases. | `GET /v1/redis/databases` |
| `redis_get_database` | A Redis database's settings and connection (credentials need reveal). | `GET /v1/redis/databases/{database}` |
| `redis_overview` | What roll Redis offers here: version, region, defaults, endpoints, pricing. | `GET /v1/redis` |
| `redis_create_replica` | A read replica of a database (its password is a credential). | `POST /v1/redis/databases/{database}/replicas` |
| `redis_delete_replica` | Remove a read replica. | `DELETE /v1/redis/replicas/{replica}` |
| `redis_create_database` | A new serverless Redis database. | `POST /v1/redis/databases` |
| `redis_delete_database` | Delete a database and its data. Irreversible. | `DELETE /v1/redis/databases/{database}` |
| `redis_create_branch` | A fork of a database's data, now or at a point in time. | `POST /v1/redis/databases/{database}/branches` |
| `redis_command` | Run one command on a database, e.g. ["GET", "key"]. Read-only commands are reads; FLUSHDB and FLUSHALL delete. | `POST /v1/redis/databases/{database}/command` |
| `redis_restore` | Rewind a database in place to a point in time. | `POST /v1/redis/databases/{id}/restore` |
| `redis_usage` | A database's usage report. | `GET /v1/redis/databases/{database}/usage` |
| `redis_update_database` | Change a database's name, memory limit, eviction policy, durability, idle timeout or history: what's left out stays. | `PATCH /v1/redis/databases/{database}` |
| `redis_list_replicas` | A database's read replicas and their lag. | `GET /v1/redis/databases/{database}/replicas` |
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
| `storage_usage_all` | Storage and requests across every bucket. | `GET /v1/storage/usage` |
| `storage_list_accounts` | Storage's accounts (12-digit ids): name, alias, the default. | `GET /v1/storage/accounts` |
| `storage_overview` | What roll storage offers here: the S3 endpoint, region and the account's ids. | `GET /v1/storage` |
| `storage_get_account` | One storage account. | `GET /v1/storage/accounts/{id}` |
| `storage_list_keys` | An account's access keys: permission, buckets, IAM user, last use (never a secret). | `GET /v1/storage/keys` |
| `storage_revoke_key` | Revoke an S3 access key at once. | `DELETE /v1/storage/keys/{id}` |
| `storage_list_roles` | An account's IAM roles: ARN, trust, permissions policy, session length. | `GET /v1/storage/roles` |
| `storage_get_role` | One IAM role. | `GET /v1/storage/roles/{name}` |
| `storage_revoke_role_sessions` | Deny every session issued for a role until now. | `POST /v1/storage/roles/{name}/revoke` |
| `storage_list_replication_targets` | Buckets outside roll that replication copies to (never the secret). | `GET /v1/storage/replication-targets` |
| `storage_get_replication_target` | One replication target (never its secret). | `GET /v1/storage/replication-targets/{name}` |
| `storage_update_bucket` | Change a bucket's versioning, lifecycle rules, CORS, trash days, tags or whether it's public: what's left out stays. | `PATCH /v1/storage/buckets/{bucket}` |
| `storage_delete_objects` | Delete these keys, or every object under a prefix (to the trash). | `POST /v1/storage/buckets/{bucket}/delete` |
| `streams_list` | Kafka-compatible streams. | `GET /v1/streams` |
| `streams_overview` | What roll streams offers here: bootstrap address, SASL, pricing. | `GET /v1/kafka` |
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
| `streams_list_keys` | The SASL users a stream's clients sign in as (never a password). | `GET /v1/streams/{stream}/keys` |
| `streams_revoke_key` | Revoke a stream's SASL user at once. | `DELETE /v1/streams/{stream}/keys/{username}` |
| `streams_update` | Change some of a stream's settings (as streams_get shows them: retention, throughput limits…); the rest stay. | `PATCH /v1/streams/{stream}` |
| `streams_update_topic` | Grow a topic's partitions or change its configs, online. | `PATCH /v1/streams/{stream}/topics/{topic}` |
| `streams_delete_group` | Delete a group and its committed offsets (refused while it has members). | `DELETE /v1/streams/{stream}/groups/{group}` |
| `nosql_list_databases` | ScyllaDB/Cassandra-compatible databases. | `GET /v1/nosql/databases` |
| `nosql_overview` | What roll NoSQL offers here: dialects, versions, endpoints. | `GET /v1/nosql` |
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
| `nosql_update_database` | Change a database's name, idle timeout, durability, history, expiry, read-only flag or masking policy: what's left out stays. Loosening masking or read-only needs admin. | `PATCH /v1/nosql/databases/{database}` |
| `nosql_activity` | What a database is doing now: connected clients, slow queries, warnings and metrics. | `GET /v1/nosql/databases/{database}/activity` |
| `nosql_list_imports` | A database's imports (cql://, s3://, file://): state, rows, tables, errors, schedule (the source's password hidden). | `GET /v1/nosql/databases/{id}/imports` |
| `queues_overview` | The broker's totals and listeners. | `GET /v1/queues/overview` |
| `queues_list_vhosts` | RabbitMQ-compatible virtual hosts. | `GET /v1/queues/vhosts` |
| `queues_create_vhost` | A new virtual host. | `POST /v1/queues/vhosts` |
| `queues_delete_vhost` | Delete a virtual host and everything in it. | `DELETE /v1/queues/vhosts/{vhost}` |
| `queues_suspend_vhost` | Scale a virtual host to zero now (a client connecting wakes it). | `POST /v1/queues/vhosts/{vhost}/suspend` |
| `queues_wake_vhost` | Start a suspended virtual host now. | `POST /v1/queues/vhosts/{vhost}/wake` |
| `queues_list_queues` | A virtual host's queues with message counts. | `GET /v1/queues/vhosts/{vhost}/queues` |
| `queues_declare_queue` | Create a queue. | `POST /v1/queues/vhosts/{vhost}/queues` |
| `queues_delete_queue` | Delete a queue and its messages. | `DELETE /v1/queues/vhosts/{vhost}/queues/{queue}` |
| `queues_purge_queue` | Drop every message in a queue. | `POST /v1/queues/vhosts/{vhost}/queues/{queue}/purge` |
| `queues_get_queue` | One queue: messages, consumers, prefetch, arguments, dead-letter queues. | `GET /v1/queues/vhosts/{vhost}/queues/{queue}` |
| `queues_queue_rates` | One queue's rates and depth over the last minutes (`queues_rates` is the broker's). | `GET /v1/queues/vhosts/{vhost}/queues/{queue}/rates` |
| `queues_list_exchanges` | A virtual host's exchanges. | `GET /v1/queues/vhosts/{vhost}/exchanges` |
| `queues_list_bindings` | Which exchange routes to which queue by which key. | `GET /v1/queues/vhosts/{vhost}/bindings` |
| `queues_declare_exchange` | Make an exchange (idempotent; amq.* reserved). | `POST /v1/queues/vhosts/{vhost}/exchanges` |
| `queues_delete_exchange` | Delete an exchange and its bindings. | `DELETE /v1/queues/vhosts/{vhost}/exchanges/{exchange}` |
| `queues_bind` | Route an exchange's messages to a queue or exchange by a key. | `POST /v1/queues/vhosts/{vhost}/bindings` |
| `queues_unbind` | Remove a binding. | `DELETE /v1/queues/vhosts/{vhost}/bindings` |
| `queues_replay` | Publish a queue's messages again (a dead-letter queue's, back where they came from). | `POST /v1/queues/vhosts/{vhost}/queues/{queue}/replay` |
| `queues_peek` | Messages in a queue, without consuming them. | `GET /v1/queues/vhosts/{vhost}/queues/{queue}/messages` |
| `queues_publish` | Publish to an exchange with a routing key. | `POST /v1/queues/vhosts/{vhost}/publish` |
| `queues_list_connections` | Open client connections: who, from where, and their channels. | `GET /v1/queues/connections` |
| `queues_close_connection` | Close a client's connection (it may reconnect). | `DELETE /v1/queues/connections/{id}` |
| `queues_list_channels` | Open channels with their prefetch and unacknowledged messages. | `GET /v1/queues/channels` |
| `queues_list_policies` | Policies (message TTL, dead-lettering, length limits) and the queues they match. | `GET /v1/queues/policies` |
| `queues_list_users` | The broker's users with their tags, permissions per vhost and limits (never a password). | `GET /v1/queues/users` |
| `queues_set_policy` | Create or replace a policy: a pattern of queue or exchange names and what applies to them. | `PUT /v1/queues/policies/{vhost}/{name}` |
| `queues_delete_policy` | Delete a policy. | `DELETE /v1/queues/policies/{vhost}/{name}` |
| `queues_rates` | Publish, deliver and ack rates over the last minutes. | `GET /v1/queues/rates` |
| `queues_usage` | Messages and storage by day. | `GET /v1/queues/usage` |
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
| `tasks_list_apps` | Synced apps: URL, functions, SDK, last sync. | `GET /v1/tasks/apps` |
| `tasks_delete_app` | Stop syncing an app whose functions tasks runs. | `DELETE /v1/tasks/apps/{id}` |
| `tasks_list_api_keys` | Tasks' API keys: name, prefix, made, last used (never the key). | `GET /v1/tasks/api-keys` |
| `tasks_delete_api_key` | Revoke an API key at once. | `DELETE /v1/tasks/api-keys/{id}` |
| `tasks_list_queues` | Queues with their parallelism, rate and backlog. | `GET /v1/tasks/queues` |
| `tasks_stats` | Nodes, queue stats, dead letters, schedules and functions. | `GET /v1/tasks/stats` |
| `tasks_list_dead_letters` | Messages that ran out of retries, newest first. | `GET /v1/tasks/dlq` |
| `tasks_delete_dead_letter` | Drop a dead-lettered message for good. | `DELETE /v1/tasks/dlq/{id}` |
| `tasks_get_queue` | A queue's parallelism, rate, retries, backlog and whether it's paused. | `GET /v1/tasks/queues/{name}` |
| `tasks_configure_queue` | Make a queue, or change its limits: what's left out stays as it is. | `PUT /v1/tasks/queues/{name}` |
| `tasks_pause_queue` | Hold a queue's deliveries; messages keep arriving. | `POST /v1/tasks/queues/{name}/pause` |
| `tasks_resume_queue` | Deliver a paused queue's messages again. | `POST /v1/tasks/queues/{name}/resume` |
| `tasks_list_topics` | Topics: a message published to one goes to each of its endpoints. | `GET /v1/tasks/topics` |
| `tasks_get_topic` | A topic and its endpoints. | `GET /v1/tasks/topics/{name}` |
| `tasks_set_topic` | A topic with exactly these endpoints. | `PUT /v1/tasks/topics/{name}` |
| `tasks_delete_topic` | Delete a topic; its pending messages stay. | `DELETE /v1/tasks/topics/{name}` |
| `tasks_delete_queue` | Delete a queue; one with unfinished messages only with `force` (which cancels them). | `DELETE /v1/tasks/queues/{name}` |
| `tasks_cancel_messages` | Cancel many messages: ids, or a queue's, URL's, topic's, label's or schedule's (or all). | `POST /v1/tasks/messages/cancel` |
| `tasks_send_event` | An event that starts the functions it triggers (an id makes it idempotent). | `POST /v1/tasks/events` |
| `tasks_list_events` | Events received, newest first. | `GET /v1/tasks/events` |
| `tasks_get_event` | An event and the runs it started. | `GET /v1/tasks/events/{id}` |
| `tasks_get_function` | A durable function: its triggers, concurrency, retries and whether it's paused. | `GET /v1/tasks/functions/{id}` |
| `tasks_pause_function` | Start no new runs of a function (running ones carry on). | `POST /v1/tasks/functions/{id}/pause` |
| `tasks_resume_function` | Start runs of a paused function again. | `POST /v1/tasks/functions/{id}/resume` |
| `tasks_cancel_function_runs` | Stop every running run of a function, or those an expression matches. | `POST /v1/tasks/functions/{id}/cancel` |
| `tasks_wait_run` | A run once it finishes, or as it is when the timeout passes. | `GET /v1/tasks/runs/{id}/wait` |
| `tasks_get_schedule` | A schedule, its next run and what it publishes. | `GET /v1/tasks/schedules/{id}` |
| `tasks_update_schedule` | Change a schedule's cron, time zone, what it publishes or its label: what's left out stays. | `PATCH /v1/tasks/schedules/{id}` |
| `tasks_pause_schedule` | Stop a schedule firing until it's resumed. | `POST /v1/tasks/schedules/{id}/pause` |
| `tasks_resume_schedule` | Fire a paused schedule again. | `POST /v1/tasks/schedules/{id}/resume` |
| `tasks_trigger_schedule` | Publish what a schedule publishes once, now. | `POST /v1/tasks/schedules/{id}/trigger` |
| `tasks_usage` | Messages, deliveries and runs by day, and what they cost. | `GET /v1/tasks/usage` |
| `search_list_clusters` | Elasticsearch-compatible search clusters. | `GET /v1/search/clusters` |
| `search_get_cluster` | A cluster's settings, state, indices, size, URL and branches. | `GET /v1/search/clusters/{cluster}` |
| `search_create_cluster` | A new cluster, or a branch of one (its password once, to tokens that may reveal). | `POST /v1/search/clusters` |
| `search_list_branches` | A cluster's branches. | `GET /v1/search/clusters/{id}/branches` |
| `search_list_remotes` | The clusters this one searches across: alias, cluster, key id, whether it connects (never the key). | `GET /v1/search/clusters/{id}/remotes` |
| `search_overview` | What roll search offers here: versions, endpoint, defaults, pricing. | `GET /v1/search` |
| `search_unlink_remote` | Unlink a remote cluster (its credentials go; a key roll made is revoked). | `DELETE /v1/search/clusters/{cluster}/remotes/{alias}` |
| `search_revoke_key` | Revoke a cluster API key at once. | `DELETE /v1/search/clusters/{cluster}/keys/{key}` |
| `search_list_browser_keys` | A cluster's browser keys that still work: name, indices or application, filter, rate limit, expiry (never the key). | `GET /v1/search/clusters/{cluster}/browser-keys` |
| `search_revoke_browser_key` | Revoke a browser key at once. | `DELETE /v1/search/clusters/{cluster}/browser-keys/{key}` |
| `search_list_inference_secrets` | Inference endpoints keyed by a roll secret, and what each follows (never the value). | `GET /v1/search/clusters/{id}/inference/secrets` |
| `search_branch_cluster` | A copy-on-write branch of a cluster. | `POST /v1/search/clusters/{cluster}/branches` |
| `search_update_cluster` | A cluster's idle timeout, request limits, flavor, IP allowlist (admin). | `PATCH /v1/search/clusters/{id}` |
| `search_delete_cluster` | Delete a cluster (its branches keep working); kept for the restore window. | `DELETE /v1/search/clusters/{id}` |
| `search_restore_cluster` | A deleted cluster back as it was, within its restore window (write). | `POST /v1/search/clusters/{id}/restore` |
| `search_cluster_metrics` | A cluster's request rate, latency percentiles and answers by kind (`range` 1h, 24h, 14d). | `GET /v1/search/clusters/{id}/metrics` |
| `search_slowlog` | A cluster's slowest recent requests. | `GET /v1/search/clusters/{id}/slowlog` |
| `search_audit` | A cluster's security audit log (failed logins, access denials, changes to users, roles and keys; never a secret). | `GET /v1/search/clusters/{id}/audit` |
| `search_analytics` | What a cluster's users search for: top queries, queries with no results, click-through (indices that opt in). | `GET /v1/search/clusters/{id}/analytics` |
| `search_suggestions` | An index's query suggestions from its search analytics: settings, last build, heaviest suggestions. | `GET /v1/search/clusters/{cluster}/indices/{index}/suggestions` |
| `search_rebuild_suggestions` | Rebuild an index's suggestions index from its analytics now (write). | `POST /v1/search/clusters/{cluster}/indices/{index}/suggestions/rebuild` |
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
| `secrets_project` | A project's environments, previews and history window. | `GET /v1/secrets/projects/{project}` |
| `secrets_get_environment` | One environment's settings. | `GET /v1/secrets/projects/{project}/environments/{env}` |
| `secrets_ensure_environment` | Make an environment, or check it matches (`replace` changes it). | `PUT /v1/secrets/projects/{project}/environments/{env}` |
| `secrets_update_environment` | Rename an environment, or set its expiry or max age. | `PATCH /v1/secrets/projects/{project}/environments/{env}` |
| `secrets_delete_environment` | Delete an environment and its secrets. | `DELETE /v1/secrets/projects/{project}/environments/{env}` |
| `secrets_delete_ephemeral_environments` | Delete ephemeral environments with a label. | `DELETE /v1/secrets/projects/{project}/environments` |
| `secrets_order_environments` | The environments' order. | `PUT /v1/secrets/projects/{project}/environments/order` |
| `secrets_create_folder` | Make a folder. | `POST /v1/secrets/projects/{project}/environments/{env}/folders` |
| `secrets_delete_folder` | Delete a folder and its keys. | `DELETE /v1/secrets/projects/{project}/environments/{env}/folders` |
| `secrets_set_imports` | What a folder imports from, replacing the list. | `PUT /v1/secrets/projects/{project}/environments/{env}/imports` |
| `secrets_set_required` | The keys every environment needs, replacing the list. | `PUT /v1/secrets/projects/{project}/required` |
| `secrets_apply_changes` | Set and delete several keys in one change. | `POST /v1/secrets/projects/{project}/environments/{env}/changes` |
| `secrets_revoke_share` | Revoke a one-off share link. | `DELETE /v1/secrets/projects/{project}/shares/{id}` |
| `secrets_create_transit_key` | Make a transit key (roll's own material). | `POST /v1/secrets/projects/{project}/transit` |
| `secrets_rotate_transit_key` | A new version of a transit key. | `POST /v1/secrets/projects/{project}/transit/{name}/rotate` |
| `secrets_update_transit_key` | The oldest version that still decrypts. | `PATCH /v1/secrets/projects/{project}/transit/{name}` |
| `secrets_backup_schedule` | Where and how often a project is backed up (never set here). | `GET /v1/secrets/projects/{project}/backup/schedule` |
| `secrets_list` | Secret names and metadata in an environment (never values). | `GET /v1/secrets/projects/{project}/environments/{env}/secrets` |
| `secrets_get` | One secret's metadata: type, version, link, tags, who changed it (never its value). | `GET /v1/secrets/projects/{project}/environments/{env}/secrets/{name}` |
| `secrets_grid` | Every key in every environment: same, different or missing (by fingerprint), required. | `GET /v1/secrets/projects/{project}/grid` |
| `secrets_imports` | What an environment's folder imports from. | `GET /v1/secrets/projects/{project}/environments/{env}/imports` |
| `secrets_exports` | Which projects a folder is shared with. | `GET /v1/secrets/projects/{project}/environments/{env}/exports` |
| `secrets_shared` | Folders other projects share with this one. | `GET /v1/secrets/projects/{project}/shared` |
| `secrets_list_shares` | A project's one-off share links: views left, expiry, opened (never the link). | `GET /v1/secrets/projects/{project}/shares` |
| `secrets_reveal` | Read a secret's value. | `GET /v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value` |
| `secrets_set` | Create or change a secret's value. | `PUT /v1/secrets/projects/{project}/environments/{env}/secrets/{name}` |
| `secrets_import` | A .env file (or JSON) into an environment as one change; a change request on a protected one. | `POST /v1/secrets/projects/{project}/environments/{env}/import` |
| `secrets_rollback` | An environment's values back to a change or a time, as a new change (destructive). | `POST /v1/secrets/projects/{project}/environments/{env}/rollback` |
| `secrets_create_environment` | A new environment (project admin). | `POST /v1/secrets/projects/{project}/environments` |
| `secrets_link_targets` | The resources a key can link to, with their fields. | `GET /v1/secrets/projects/{project}/links` |
| `secrets_link` | Link a key to a resource (or unlink it). | `PUT /v1/secrets/projects/{project}/environments/{env}/secrets/{name}` |
| `secrets_delete` | Delete a secret. | `DELETE /v1/secrets/projects/{project}/environments/{env}/secrets/{name}` |
| `secrets_list_aws_roles` | A project's registered AWS roles for dynamic AWS keys (registering is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/aws-roles` |
| `secrets_list_outside_databases` | Servers roll doesn't run (Postgres, MySQL, SQL Server, ClickHouse, RabbitMQ, Redis, MongoDB) whose users a project rotates (never the admin URL; adding one is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/outside-databases` |
| `secrets_list_aws_users` | A project's AWS IAM users whose keys roll rotates (never a key; adding one is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/aws-users` |
| `secrets_list_cloud_accounts` | A project's GCP and Azure accounts for dynamic credentials (no secrets; adding one is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/cloud-accounts` |
| `secrets_list_cas` | A project's own certificate authorities (public certificates, domains, limits; never a key; adding one is admin). | `GET /v1/secrets/projects/{project}/cas` |
| `secrets_list_ssh_cas` | A project's SSH CAs (public keys, principals, limits; never a private key; adding one is admin). | `GET /v1/secrets/projects/{project}/ssh-cas` |
| `secrets_totp` | The current one-time code from a secret holding an authenticator seed (a reveal; never the seed). | `GET /v1/secrets/projects/{project}/environments/{env}/secrets/{name}/totp` |
| `secrets_list_saas` | A project's rotators for third-party API keys: vendor and the account each rotates for (never a credential). | `GET /v1/secrets/projects/{project}/saas` |
| `secrets_list_kubernetes` | The Kubernetes clusters a project issues service-account tokens for: names and API servers (never a token). | `GET /v1/secrets/projects/{project}/kubernetes` |
| `secrets_list_github_apps` | A project's GitHub Apps for short-lived installation tokens (ids, repositories, permissions; never the private key; adding one is admin). | `GET /v1/secrets/projects/{project}/github-apps` |
| `secrets_list_directories` | A project's LDAP and Active Directory servers: URL, flavor, bind DN, managed accounts, groups, gateway (never the bind password; adding one is admin). | `GET /v1/secrets/projects/{project}/directories` |
| `secrets_list_snowflake_accounts` | A project's Snowflake accounts: host, admin user and role, the admin key's fingerprint, managed user, roles (never the key; adding one is admin). | `GET /v1/secrets/projects/{project}/snowflake-accounts` |
| `secrets_list_atlas_projects` | A project's MongoDB Atlas projects: group id, client id, managed users, roles (never the client secret; adding one is admin). | `GET /v1/secrets/projects/{project}/atlas-projects` |
| `secrets_list_gateways` | A project's gateways into private networks with each link's status (online, node, version, allow-list, last seen; never a token; making one is admin). | `GET /v1/secrets/projects/{project}/gateways` |
| `secrets_ca_acme` | A CA's ACME server: directory, pre-authorization, challenges, external account bindings (never an HMAC key; making one is admin). | `GET /v1/secrets/projects/{project}/cas/{id}/acme` |
| `secrets_changes` | What changed in an environment (who, when, which keys; never values). | `GET …/environments/{env}/changes` |
| `secrets_get_change` | One change: what it did to each key (fingerprints, never values). | `GET /v1/secrets/projects/{project}/environments/{env}/changes/{seq}` |
| `secrets_insights` | Each key's version, last change and read, reads in 30 days, staleness. | `GET /v1/secrets/projects/{project}/environments/{env}/insights` |
| `secrets_verify_audit` | Whether a project's secrets audit chain holds: rows walked, how far signed digests vouch, the first break. | `GET /v1/secrets/projects/{project}/audit/verify` |
| `secrets_audit` | A project's secrets audit log, newest first: who read or changed what (never a value), by env, key, actor or action. | `GET /v1/secrets/projects/{project}/audit` |
| `secrets_required` | The keys every standard environment must define. | `GET /v1/secrets/projects/{project}/required` |
| `secrets_folders` | An environment's folders. | `GET /v1/secrets/projects/{project}/environments/{env}/folders` |
| `secrets_suggestions` | Links roll suggests for a project's resources. | `GET /v1/secrets/projects/{project}/suggestions` |
| `secrets_versions` | A secret's history (fingerprints, never values). | `GET …/environments/{env}/secrets/{name}/versions` |
| `secrets_compare` | Which keys differ between two environments (never values). | `GET /v1/secrets/projects/{project}/compare` |
| `secrets_rotate` | A new value for a secret, switched to: a generated one, or a managed link's, made where it lives. | `POST …/environments/{env}/secrets/{name}/rotate` |
| `secrets_promote` | Copy another environment's secrets in (a change request into a protected one). | `POST …/environments/{env}/promote` |
| `secrets_list_requests` | An environment's change requests. | `GET …/environments/{env}/requests` |
| `secrets_request_value` | A link where a person pastes a value the agent doesn't have, sealed in their browser into the folder, never through the conversation (one answer and a day by default; it answers an `rlq_` token, and a link when roll knows the dashboard's address; either goes only to the person it's for). | `POST /v1/secrets/projects/{project}/environments/{env}/value-requests` |
| `secrets_list_value_requests` | A project's value requests and their state (open, answered, expired, cancelled; never a value). | `GET /v1/secrets/projects/{project}/value-requests` |
| `secrets_cancel_value_request` | Close a value request's link. | `DELETE /v1/secrets/projects/{project}/value-requests/{id}` |
| `secrets_get_request` | One change request: what it changes, reviews, whether approvals are met. | `GET /v1/secrets/projects/{project}/environments/{env}/requests/{id}` |
| `secrets_apply_request` | Apply an approved change request (never with bypass). | `POST /v1/secrets/projects/{project}/environments/{env}/requests/{id}/apply` |
| `secrets_close_request` | Withdraw a change request. | `POST /v1/secrets/projects/{project}/environments/{env}/requests/{id}/close` |
| `secrets_propose` | Propose changes for a person to review and apply: the way to change a protected environment. | `POST …/environments/{env}/requests` |
| `secrets_transit_encrypt` | Encrypt with a project's encryption key (the key never leaves roll). | `POST /v1/secrets/projects/{project}/transit/{name}/encrypt` |
| `secrets_transit_decrypt` | Decrypt a `roll:v<n>:` (or Vault's `vault:v<n>:`) ciphertext; reveal, audited. | `POST /v1/secrets/projects/{project}/transit/{name}/decrypt` |
| `secrets_transit_sign` | An Ed25519 signature by a project's signing key. | `POST /v1/secrets/projects/{project}/transit/{name}/sign` |
| `secrets_transit_verify` | Whether a signature is the key's. | `POST /v1/secrets/projects/{project}/transit/{name}/verify` |
| `secrets_transit_public_key` | A signing key's public keys (raw, PEM, JWK). | `GET /v1/secrets/projects/{project}/transit/{name}/public` |
| `secrets_list_transit_keys` | A project's encryption and signing keys: names, kinds, versions and dates (never key material). | `GET /v1/secrets/projects/{project}/transit` |
| `secrets_transit_rewrap` | Re-encrypt a ciphertext under the key's newest version, without the plaintext leaving roll (after a rotation). | `POST /v1/secrets/projects/{project}/transit/{name}/rewrap` |
| `secrets_transit_hmac` | An HMAC-SHA256 of input by a project's key (input is base64). | `POST /v1/secrets/projects/{project}/transit/{name}/hmac` |
| `secrets_transit_hmac_verify` | Whether an HMAC of input is the key's. | `POST /v1/secrets/projects/{project}/transit/{name}/hmac/verify` |
| `secrets_transit_datakey` | A fresh data key for encrypting locally: its plaintext and the same key encrypted by the project's key (store that one). Audited. | `POST /v1/secrets/projects/{project}/transit/{name}/datakey` |
| `secrets_version_value` | One earlier version's value of a secret (secrets_versions lists them), to compare or roll back. | `GET /v1/secrets/projects/{project}/environments/{env}/secrets/{name}/versions/{v}/value` |
| `secrets_renew_lease` | Push a dynamic credential's expiry out, as a long-running consumer does instead of restarting. | `POST /v1/secrets/projects/{project}/leases/{id}/renew` |
| `secrets_reach` | Who reaches a folder or key, where it flows, who read it (names only; admin): after a leak. | `GET /v1/secrets/projects/{project}/environments/{env}/reach` |
| `secrets_folder_policies` | An environment's approval policies per folder. | `GET /v1/secrets/projects/{project}/environments/{env}/folder-policies` |
| `secrets_list_rotators` | Which secrets roll rotates, how often. | `GET /v1/secrets/projects/{project}/rotators` |
| `secrets_list_syncs` | Where a project's environments are pushed. | `GET /v1/secrets/projects/{project}/syncs` |
| `secrets_diff` | A local .env against an environment, by fingerprint (names only). | `POST /v1/secrets/projects/{project}/environments/{env}/diff` |
| `secrets_list_rotations` | A project's secret rotations: what rotated and what's due. | `GET /v1/secrets/projects/{project}/rotations` |
| `secrets_ca_certificates` | The certificates a project's CA has issued (never a key). | `GET /v1/secrets/projects/{project}/cas/{ca}/certificates` |
| `secrets_list_placeholders` | Placeholders handed out for a project's host-bound keys (key, consumer, expiry; never the placeholder or the value). | `GET /v1/secrets/projects/{project}/placeholders` |
| `secrets_list_recoverable` | Deleted workspace projects still in their recovery window that you may recover: who deleted each, when, and when it's purged (recovering stays a person's). | `GET /v1/secrets/deleted` |
| `secrets_end_placeholder` | End one placeholder now. | `DELETE /v1/secrets/projects/{project}/placeholders/{id}` |
| `secrets_end_consumer_placeholders` | End every live placeholder a consumer holds. | `DELETE /v1/secrets/projects/{project}/placeholders?consumer=` |
| `secrets_list_leases` | A project's live leases on dynamic credentials, never a credential. | `GET /v1/secrets/projects/{project}/leases` |
| `secrets_end_lease` | End a dynamic credential's lease now (one's own with reveal). | `DELETE /v1/secrets/projects/{project}/leases/{id}` |
| `secrets_credential_check` | A credential found in code that the project doesn't hold: its kind, whether it was live when last checked, its account and rotator (never its value). | `GET /v1/secrets/projects/{project}/unstored/{fingerprint}` |
| `secrets_get_values` | All of an environment's values at once, resolved, as one reveal (`dynamic=false` leaves dynamic keys out). | `GET /v1/secrets/projects/{project}/environments/{env}/values` |
| `secrets_health` | What needs fixing in a project's secrets, by name, in the environments you may read: expired, missing, exposed, weak, expiring, stale, unread, rotations or revocations failing, certificates expiring, public (`code`: keys no repository mentions). | `GET /v1/secrets/projects/{project}/health` |
| `secrets_findings` | Every project's secrets health findings you may read. | `GET /v1/secrets/findings` |
| `secrets_find` | Where a key is set across the projects you may read. | `GET /v1/secrets/search` |
| `secrets_scan` | Which of these values are a project's stored secrets (never the value): check a diff before committing it. | `POST /v1/secrets/projects/{project}/leaks` |
| `apps_list_services` | App services; with `deleted`, those deleted in the last 48 hours (until `purge_at`). | `GET /v1/apps/services` |
| `apps_restore_service` | A service deleted in the last 48 hours back as it was, its live deployments running again (deploy). | `POST /v1/apps/services/{service}/restore` |
| `apps_create_service` | A new service in a project, from a repo, an image or an upload. | `POST /v1/apps/projects/{project}/services` |
| `apps_list_environments` | A service's environments. | `GET /v1/apps/services/{service}/environments` |
| `apps_get_service` | A service with its environments' summaries. | `GET /v1/apps/services/{service}` |
| `apps_set_environment` | Create or change an environment: branch, run settings, following an image tag, waiting for CI. | `PUT /v1/apps/services/{service}/environments/{env}` |
| `apps_update_service` | Change a service's name, source, build or run settings (run takes `alerts {memory_pct, error_pct, p95_ms}`). | `PATCH /v1/apps/services/{service}` |
| `apps_deploy` | Deploy a service to an environment: a git revision of its repo, or an image (production deploys are "deploy" actions). `override: true` goes past a deploy freeze, as do `apps_redeploy`'s, `apps_promote`'s and `apps_rollback`'s: sent as `X-Roll-Override: 1`, which roll-access makes `admin` (a deploy token can't) and binds into an approval. | `POST /v1/apps/services/{service}/environments/{env}/deploys` |
| `apps_list_deploys` | An environment's deploys, newest first. | `GET /v1/apps/services/{service}/environments/{env}/deploys` |
| `apps_deploy_logs` | A deploy's build and release log. | `GET /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/logs` |
| `apps_promote` | Deploy another environment's running image into this one (`expect`: the deployment you checked, 409 if it moved). | `POST /v1/apps/services/{service}/environments/{env}/promote` |
| `apps_cancel_deploy` | Stop a deploy that hasn't gone live. | `POST /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/cancel` |
| `apps_rollout` | Move a manual gradual rollout that waits: advance, all, or abort (deploy). | `POST /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollout` |
| `apps_sleep` | Stop an environment's instances until woken. | `POST /v1/apps/services/{service}/environments/{env}/sleep` |
| `apps_wake` | Start a sleeping environment's instances. | `POST /v1/apps/services/{service}/environments/{env}/wake` |
| `apps_usage` | An environment's usage and cost per day. | `GET /v1/apps/services/{service}/environments/{env}/usage` |
| `apps_restart` | Restart an environment's instances. | `POST /v1/apps/services/{service}/environments/{env}/restart` |
| `apps_purge_cache` | Drop a web service's cached answers: some paths, answers with some cache tags, or everything (production: deploy). | `POST /v1/apps/services/{service}/environments/{env}/cache/purge` |
| `apps_restart_instance` | Replace one instance, a fresh one answering first (production: deploy). | `POST /v1/apps/services/{id}/environments/{env}/instances/{name}/restart` |
| `apps_redeploy` | Deploy what's running again with the current settings and variables. | `POST /v1/apps/services/{service}/environments/{env}/redeploy` |
| `apps_run_job` | Run a one-off command in an environment's image with its variables, in up to 100 tasks (`parallelism`, `retries`). | `POST /v1/apps/services/{service}/environments/{env}/jobs` |
| `apps_job_logs` | A job's output. | `GET /v1/apps/services/{service}/environments/{env}/jobs/{job}/logs` |
| `apps_list_jobs` | An environment's one-off jobs, newest first. | `GET /v1/apps/services/{service}/environments/{env}/jobs` |
| `apps_get_job` | One job: status, exit code, timing. | `GET /v1/apps/services/{service}/environments/{env}/jobs/{job}` |
| `apps_wait_job` | A job once it ends: a task that waits as long as it takes for a client that runs tasks, its status now for any other. | `GET /v1/apps/services/{service}/environments/{env}/jobs/{job}` |
| `apps_create_sandbox` | A short-lived container to run commands in: the live image or a public one, without the environment's variables or network unless asked (a closed environment's on its network); admin. | `POST /v1/apps/services/{service}/environments/{env}/sandboxes` |
| `apps_list_sandboxes` | An environment's sandboxes and when each ends. | `GET /v1/apps/services/{service}/environments/{env}/sandboxes` |
| `apps_get_sandbox` | One sandbox. | `GET /v1/apps/services/{service}/environments/{env}/sandboxes/{sandbox}` |
| `apps_sandbox_exec` | A command run in a sandbox: exit code, stdout, stderr (1 MiB each), killed at its timeout; admin. | `POST /v1/apps/services/{service}/environments/{env}/sandboxes/{sandbox}/exec` |
| `apps_delete_sandbox` | Stop and remove a sandbox now; admin. | `DELETE /v1/apps/services/{service}/environments/{env}/sandboxes/{sandbox}` |
| `apps_cancel_job` | Stop a running job (production: deploy). | `POST /v1/apps/services/{service}/environments/{env}/jobs/{job}/cancel` |
| `apps_events` | What happened to services, in order after a cursor. | `GET /v1/apps/events` |
| `apps_list_variables` | An environment's variables, values masked. | `GET /v1/apps/services/{service}/environments/{env}/variables` |
| `apps_set_variables` | Set or unset an environment's variables, or copy some from another (`copy_from` with `copy`; reveal on that environment too). `seal` hides values for good, `files` keeps some as files in the instances, `runtime_only` keeps some out of builds. Production is a "deploy" action. | `PATCH /v1/apps/services/{service}/environments/{env}/variables` |
| `apps_diff_environments` | How an environment differs from another: variables missing, extra or different (names only; `differ` needs reveal on both) and settings with the body that matches each. | `GET /v1/apps/services/{service}/environments/{env}/diff` |
| `apps_metrics` | CPU, memory and requests of an environment, and each instance's own last hour (`by_instance`). | `GET /v1/apps/services/{service}/environments/{env}/metrics` |
| `apps_plan` | How roll would build and run a repo, before making a service (a dry run). | `POST /v1/apps/plan` |
| `apps_errors` | An environment's errors, grouped, the most frequent first. | `GET /v1/apps/services/{service}/environments/{env}/errors` |
| `apps_push_status` | Each run.push rule's lag, deliveries, dead letters and failures. | `GET /v1/apps/services/{service}/environments/{env}/push` |
| `apps_set_error_state` | Resolve, mute or reopen an error group (a resolved one seen again reopens as a regression). | `PATCH /v1/apps/services/{service}/environments/{env}/errors/{fingerprint}` |
| `apps_rotate_config` | A new read token for an environment's runtime config (the old one reads until a redeploy). | `POST /v1/apps/services/{service}/environments/{env}/config/rotate` |
| `apps_traces` | Recent traces (root span, duration, failures); `min_ms` keeps the slow ones. | `GET /v1/apps/services/{service}/environments/{env}/traces` |
| `apps_get_trace` | One trace's spans, in start order. | `GET /v1/apps/services/{service}/environments/{env}/traces/{trace}` |
| `apps_app_metrics` | The app's own OpenTelemetry metrics, as series. | `GET /v1/apps/services/{service}/environments/{env}/app-metrics` |
| `apps_analytics` | Page views, visitors, top pages, referrers, countries. | `GET /v1/apps/services/{service}/environments/{env}/analytics` |
| `apps_vitals` | What visitors' browsers measured on an environment's pages: Web Vitals p75s, rated, overall and per route. | `GET /v1/apps/services/{service}/environments/{env}/vitals` |
| `apps_route_stats` | An environment's routes over the last minutes: requests, errors, p50/p95/max latency (apps-next). | `GET /v1/apps/services/{id}/environments/{env}/routes` |
| `apps_import` | What a roll.toml, a docker-compose file or another platform's file (Render's render.yaml, fly.toml, railway.json, Heroku's app.json and Procfile) would make, without making anything; or a roll code repo's or a public Git repository's root roll.toml (`repo`, `git`). | `POST /v1/apps/import` |
| `apps_export_project` | A project as a roll.toml, variables by name only (apps #26). | `POST /v1/apps/projects/{project}/export` |
| `apps_get_environment` | One environment: its URL, state, live deploy, branch and size; `build_closed` and `base_updated`. | `GET /v1/apps/services/{id}/environments/{env}` |
| `apps_delete_environment` | Delete an environment with its deploys and domains. | `DELETE /v1/apps/services/{id}/environments/{env}` |
| `apps_link` | Give a service a roll resource's connection variables (reveal on the resource). | `PUT /v1/apps/services/{id}/links/{kind}/{ref}` |
| `apps_unlink` | Stop giving a service a linked resource's variables. | `DELETE /v1/apps/services/{id}/links/{kind}/{ref}` |
| `apps_verify_domain` | Check a custom domain's DNS now. | `POST /v1/apps/services/{id}/environments/{env}/domains/{host}/verify` |
| `apps_list_backups` | An environment's volume backups. | `GET /v1/apps/services/{id}/environments/{env}/volume/backups` |
| `apps_backup_volume` | Back up a volume now. | `POST /v1/apps/services/{id}/environments/{env}/volume/backups` |
| `apps_restore_backup` | Put a volume back as a backup had it (delete: it replaces the files); `from` another environment's backup seeds this one (reveal there too). | `POST /v1/apps/services/{id}/environments/{env}/volume/backups/{bid}/restore` |
| `apps_activity` | Who changed what on a service (never a value). | `GET /v1/apps/services/{service}/activity` |
| `apps_get_deploy` | One deploy. | `GET …/environments/{env}/deploys/{deploy}` |
| `apps_wait_deploy` | A deploy once it's live or failed, or built and waiting at `ready` for people's approvals (apps #576; a person approves, never a token): a task that waits as long as it takes for a client that runs tasks, its status now for any other. | `GET …/environments/{env}/deploys/{deploy}` |
| `apps_list_links` | What a service is linked to. | `GET /v1/apps/services/{service}/links` |
| `apps_list_cron_runs` | A cron service's runs. | `GET …/environments/{env}/runs` |
| `apps_get_cron_run` | One cron run: status, exit code, times. | `GET /v1/apps/services/{id}/environments/{env}/runs/{run}` |
| `apps_cron_run_logs` | A cron run's output (read: its lines are in the environment's log). | `GET /v1/apps/services/{id}/environments/{env}/runs/{run}/logs` |
| `apps_cancel_cron_run` | Stop a running cron run (production: deploy). | `POST /v1/apps/services/{id}/environments/{env}/runs/{run}/cancel` |
| `apps_run_cron` | Run a cron job now (deploy in production). | `POST …/environments/{env}/runs` |
| `apps_limits` | A project's apps spend limits. | `GET /v1/apps/projects/{project}/limits` |
| `apps_project_usage` | A month's apps cost by service and environment (previews and deleted services as a row each), most costly first, this month's projected. | `GET /v1/apps/projects/{project}/usage` |
| `apps_set_limits` | Set a project's monthly apps alert and hard cap (the cap pauses the project; admin). | `PUT /v1/apps/projects/{project}/limits` |
| `apps_overview` | What roll apps runs here: domain, runtime, prices, every service. | `GET /v1/apps` |
| `apps_list_project_services` | The services in one workspace project. | `GET /v1/apps/projects/{project}/services` |
| `apps_list_notifications` | Where a project's deploy, crash and preview events are sent (webhook, Slack, Discord, email; the URL's host only). | `GET /v1/apps/projects/{project}/notifications` |
| `apps_add_notification` | Notify a webhook, Slack, Discord, the project's owner by email, PagerDuty (`routing_key`) or Opsgenie (`api_key`) of a project's events; URLs and keys are stored sealed and never shown again (and redacted from the audit log, #586). | `POST /v1/apps/projects/{project}/notifications` |
| `apps_update_notification` | Change which events, environments and services a notification gets: given ones replace. | `PATCH /v1/apps/projects/{project}/notifications/{nid}` |
| `apps_delete_notification` | Stop sending a project's events there. | `DELETE /v1/apps/projects/{project}/notifications/{nid}` |
| `apps_test_notification` | Send a test event now (not retried): whether it arrived. | `POST /v1/apps/projects/{project}/notifications/{nid}/test` |
| `apps_notification_deliveries` | A notification's last 50 deliveries, newest first: what was sent and whether it arrived. | `GET /v1/apps/projects/{project}/notifications/{nid}/deliveries` |
| `apps_list_drains` | Where a project's logs are sent (the URL's host only). | `GET /v1/apps/projects/{project}/drains` |
| `apps_add_drain` | Send every matching service's logs out of roll: an HTTPS endpoint (signed; its signing secret comes back once), an OTLP collector, syslog over TLS, or an S3 bucket (apps #580; the AWS keys go as headers, which the audit log redacts), or the project's own roll storage bucket with nothing to paste (`bucket`, apps #590). | `POST /v1/apps/projects/{project}/drains` |
| `apps_update_drain` | Change what a drain sends or its headers: given ones replace (a new URL is a new drain). | `PATCH /v1/apps/projects/{project}/drains/{did}` |
| `apps_delete_drain` | Stop sending a project's logs there. | `DELETE /v1/apps/projects/{project}/drains/{did}` |
| `apps_test_drain` | Send one signed test line now (not retried): whether it arrived. | `POST /v1/apps/projects/{project}/drains/{did}/test` |
| `apps_pending` | What the next deploy changes: variables (names), run settings, project secrets. | `GET /v1/apps/services/{service}/environments/{env}/pending` |
| `apps_deploy_commits` | The commits a deploy brings over the one live before it. | `GET /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/commits` |
| `apps_deploy_vulnerabilities` | Known vulnerabilities in a deploy's image, most severe first (`severity` keeps that or worse), with the scan's counts. | `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}/vulnerabilities` |
| `apps_explain_deploy` | Why a failed deploy failed, from the repo owner's model (kept; `again` asks anew). | `POST /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/explain` |
| `apps_deploy_sbom` | A deploy's image as a CycloneDX SBOM (every package and version). | `GET /v1/apps/services/{id}/environments/{env}/deploys/{dep}/sbom` |
| `apps_status_page` | A project's public status page: title, URL, what it shows. | `GET /v1/apps/projects/{project}/status-page` |
| `apps_remove_status_page` | Take a project's public status page down (making or changing one is a person's: admin, no tool). | `DELETE /v1/apps/projects/{project}/status-page` |
| `apps_list_incidents` | Its incidents: the last 90 days' and the open ones, with updates. | `GET /v1/apps/projects/{project}/status-page/incidents` |
| `apps_create_incident` | Post an incident on the public page (published text: the credential guard refuses credentials as where people read). | `POST /v1/apps/projects/{project}/status-page/incidents` |
| `apps_update_incident` | Add a public update to an incident (`resolved` closes it). | `POST /v1/apps/projects/{project}/status-page/incidents/{iid}/updates` |
| `apps_environment_slo` | An environment's SLO: per indicator, the share that met it, the error budget left, and whether it's burning. | `GET /v1/apps/services/{id}/environments/{env}/slo` |
| `apps_list_forms` | A static site's forms with their counts (never what people sent). | `GET /v1/apps/services/{id}/environments/{env}/forms` |
| `apps_form_submissions` | What people sent through a form, newest first (reveal; every read audited). | `GET /v1/apps/services/{id}/environments/{env}/forms/{form}/submissions` |
| `apps_delete_form_submission` | Delete one submission for good. | `DELETE /v1/apps/services/{id}/environments/{env}/forms/{form}/submissions/{sub}` |
| `apps_rollback` | Roll an environment back to an earlier deploy, with the variables it went live with. | `POST /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollback` |
| `apps_rollback_previous` | Put back the deploy that was live before this one. | `POST /v1/apps/services/{service}/environments/{env}/rollback` |
| `apps_logs` | Recent logs of a service's environment, or with `http` its requests (filter by `status`, `min_ms`). | `GET /v1/apps/services/{service}/environments/{env}/logs` |
| `apps_list_domains` | An environment's domains. | `GET /v1/apps/services/{service}/environments/{env}/domains` |
| `apps_add_domain` | Point a domain at an environment. | `POST /v1/apps/services/{service}/environments/{env}/domains` |
| `apps_remove_domain` | Remove a domain. | `DELETE /v1/apps/services/{service}/environments/{env}/domains/{host}` |
| `apps_update_domain` | Redirect a domain to another hostname of the environment, or serve another port on it. | `PATCH /v1/apps/services/{service}/environments/{env}/domains/{host}` |
| `apps_preview_domain` | A project's preview domain: the wildcard its preview environments answer under. | `GET /v1/apps/projects/{project}/preview-domain` |
| `apps_set_preview_domain` | Give a project's previews a wildcard of its own (answers the DNS record). | `POST /v1/apps/projects/{project}/preview-domain` |
| `apps_verify_preview_domain` | Check a preview domain's DNS now. | `POST /v1/apps/projects/{project}/preview-domain/verify` |
| `apps_remove_preview_domain` | Remove a project's preview domain. | `DELETE /v1/apps/projects/{project}/preview-domain` |
| `previews_list` | Preview environments. | `GET /v1/previews` |
| `previews_get` | A preview with its branched resources and its schema changes. | `GET /v1/previews/{id}` |
| `previews_wait` | A preview once it's ready, failed, sleeping or waiting on a person (a task waits as long as it takes). | `GET /v1/previews/{id}` |
| `previews_create` | A preview environment for a change or a branch: the project's resources branched, its services deployed. | `POST /v1/previews` |
| `previews_refresh` | Redeploy a preview now. | `POST /v1/previews/{id}/refresh` |
| `previews_errors` | A preview's errors, grouped (masked as logs are). | `GET /v1/previews/{id}/errors` |
| `previews_traces` | A preview's recent traces: root span, service, duration, spans, failed (masked). | `GET /v1/previews/{id}/traces` |
| `previews_routes` | Each route's p50/p95 in a preview against main's preview, the slower ones first (masked). | `GET /v1/previews/{id}/routes` |
| `previews_get_trace` | One trace's spans, oldest first (masked). | `GET /v1/previews/{id}/traces/{trace}` |
| `previews_mask_suggestions` | Columns that look like people's data and aren't masked in previews, with the rule each would take. | `GET /v1/previews/projects/{project}/mask-suggestions` |
| `previews_metrics` | Each of a preview's services' CPU and memory now. | `GET /v1/previews/{id}/metrics` |
| `previews_logs` | A preview's deploy and service logs. | `GET /v1/previews/{id}/logs` |
| `previews_requests` | A preview's latest 200 requests through roll (service, method, path, status, time), to find the one that failed. | `GET /v1/previews/{id}/requests` |
| `previews_reset` | Branch a preview's resources again from their parents. | `POST /v1/previews/{id}/reset` |
| `previews_keep` | Keep a preview past its time to live, for some hours. | `POST /v1/previews/{id}/keep` |
| `previews_activity` | What happened to a preview, newest first. | `GET /v1/previews/{id}/activity` |
| `previews_share` | A link for someone outside the team, for some hours (reveal: its token is in the answer). | `POST /v1/previews/{id}/shares` |
| `previews_revoke_share` | Stop a preview's share link at once. | `DELETE /v1/previews/{id}/shares/{share}` |
| `previews_settings` | A project's preview rules (changing them is admin, in the dashboard). | `GET /v1/previews/projects/{project}/settings` |
| `previews_set_rules` | Change which changes get a preview, how long they live, budgets (access, secrets, egress, variables, masking, the visual review, GitHub and jobs stay a person's). | `PUT /v1/previews/projects/{project}/settings` |
| `previews_visitors` | The last people signed in to roll who opened a preview. | `GET /v1/previews/{id}/visitors` |
| `previews_list_hooks` | A project's preview deploy hooks (never their secrets). | `GET /v1/previews/projects/{project}/hooks` |
| `previews_delete_hook` | Delete a preview deploy hook. | `DELETE /v1/previews/projects/{project}/hooks/{hook}` |
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

1. **Housekeeping before deadlines (this PR: client ID metadata documents against draft -03).**
   VS Code's new files landed (#694). Now draft-ietf-oauth-client-id-metadata-document-03
   (2026-11-02): a client id with userinfo or not in normal form refused (the URL fetched is the one
   compared), documents with `client_secret`, with `token_endpoint_auth_methods_supported` lacking
   `none`, or with grant or response types leaving nothing in common refused, a loopback redirect
   matched on any port (RFC 8252 7.3; the consent screen too), a cached document kept through a
   transient fetch failure, and roll's own issuer refused as a client id. Not done: the draft's 5 KB
   read limit (roll reads 64 KiB: real clients' documents haven't been measured) and its cache
   headers (roll re-fetches hourly).
2. **MCP Events, more feeds and webhook mode.** Poll mode landed with `apps.event` (#692; see
   "Events"). Each next feed needs its product's log after a cursor: roll code's durable land log
   (`Store::land_log`, no route yet) and broken-main items, previews becoming ready, approvals
   decided (roll-access's, this thread's), secrets' leaks. Then webhook mode on the existing
   delivery queue (Standard Webhooks signing, a verification challenge, `roll_net::PublicOnly`),
   which ChatGPT takes; Claude Code has an open request.
3. **Decision for the user: list roll's server in the MCP Registry.** GitHub Copilot's "Registry
   only" policy blocks servers missing from the configured registry. Listing needs the `sh.roll`
   namespace proved by DNS or HTTP on roll.sh (`mcp-publisher login dns --domain roll.sh`) and
   publishes publicly under roll's name, so it waits for the user; `server.json` would point at
   `https://api.roll.sh/v1/mcp` (the server card, `/v1/mcp/server-card`, already says the same).
4. **Decision for the user: a sandboxed "code mode"** (Cloudflare's Code Mode, Anthropic's code
   execution with MCP). A model writes a short script against typed tool functions; roll runs it
   and each call still goes through the gate and the audit log as if made one by one. It cuts
   tokens and round trips for clients with many tools (search mode, `?tools=search`, already
   covers clients that can't take the list). It needs an embedded script engine on rolld (QuickJS
   through `rquickjs`, with memory and time limits and no I/O but tool calls) and runs
   model-written code on fleet nodes, which roll apps' rule keeps customer code off. Options: (a)
   build it in rolld behind a flag; (b) run scripts in a preview-style container, slower to start;
   (c) not now, search mode is enough. Not started until the user picks.
5. **As siblings land** (`sibling-check.py`, 2026-11-22 09:00, over every open PR): search's
   relevance tuning (#677: synonyms sets, query rulesets and their test, an index's settings and
   `_explain`; its rows checked, mapped once it lands, the settings route's action perhaps from its
   body). #485's migration landed with both routes kept tool-less on purpose (the remote's
   credentials go in their bodies). Secrets' Next item 3 (reach naming people, OAuth clients,
   services and sandboxes): `respond_to_leaked_secret` then names the services to redeploy from
   `secrets_reach` instead of asking which read the key. #524 (previews on members) adds no routes
   beyond #501's; #627, #629, #596 and #504 add none. Not a tool: making a search browser key
   (#358's create answers the key; `limits.max_body_bytes` has no schema to join).
6. **Suggested to other threads** (from the 2026-11-21 comparison of 11 MCP servers): fetching a
   preview's or app's URL through its access gate (Vercel's `web_fetch_vercel_url`; previews,
   apps), Postgres logs, live metrics and migrations as records (Supabase, Neon, PlanetScale), a
   read-only ES|QL route (search), log grouping and field values (apps).
7. **Later:** the every-row SQL guard: computes run stock Postgres images, which have no
   safe-update setting (no pg-safeupdate), and an event trigger neither fires for `TRUNCATE` nor
   sees a `WHERE`, so it needs Postgres's own parser in pg-control (`pg_query`: libpg_query, built
   with libclang) and would still catch only mistakes, since a `DO` block or a function runs the
   same statement unseen; branches and point-in-time restore already undo that mistake. a client config for Devin's desktop app (`mcpServers`, `url`, `transport: "http"`)
   once its tool limit and OAuth are checked against a live client; tasks' tools in areas once it
   nears ~60 (47 now), as for any product past ~60; a docs search tool (Supabase, Vercel, Neon,
   Cloudflare and Stripe have one) once roll has user documentation (`docs/` is design notes and
   contracts); a call's `_meta.traceparent` (SEP-414) carried into the routed request once rolld's
   API traces; a removed person's repositories handed to someone else (roll code's). Looked at and left by design (#631's review): a work
   item's claim is cooperative (heartbeat, release and complete check the claimant only when the
   body names an `agent`), so `PATCH /v1/code/work/{id}`'s state is no weaker than `release_work`;
   and `draft: true` on a queued change dequeues it as a `write`, which only delays a landing
   (`update_work` and `update_change` keep clear of both anyway). For their
   owners (they fail only under load and pass alone): code-control's
   `automations::concurrency_cancel_and_retry` and
   `agents_code::roll_code_projects_with_auto_pull_follow_their_default_branch`.
   Not doing: dynamic toolsets (GitHub's: the model turns toolsets on mid-session), since 2026-07-28 is
   stateless and search mode (`?tools=search`) covers clients that can't take the list; "remember this browser"; a preset that reveals development's values without asking
   (#354); a connection-level `?project=` (only a token's rules bound a project; a default that
   some tools can't honour would look like a boundary).
8. **Landed 2026-11-22 morning:** operator policy for agent connections: `agents_ask` and
   `mcp_lockdown` / `mcp_read_only` (#678), and `oauth_clients`, the OAuth clients that may connect,
   checked at consent, at every token and on every request, nothing revoked (#680). SQL sessions name who sent them (`application_name`: `roll tok_…`, so `postgres_activity` tells
   an agent's queries apart; #683), and apps' sandbox tools marked open-world (#684). Output schemas for 2026-07-28 clients, learned from the sweep's real answers (#687). MCP Events in poll mode, starting with apps' event log (#692). VS Code's config to its 1.146 files (#694). **Landed 2026-11-21:** step-up authorization for OAuth clients (#673: a refusal of a connection's own
   rules records the one rule it needed, `roll:step-up:<id>` in-band and as a 403 to Claude Code
   from 2.1.288, the consent screen adds roll's record to that connection; two security reviews); the instructions name every product by its tool prefix, search mode's tools
   always load, the 48 KiB cut's reason corrected, and Next ranked from the 2026-11-22 comparison
   (#670); workflows for storage, NoSQL and Redis (#666: `recover_deleted_objects`,
   `try_cql_change_on_branch`, `find_redis_memory_hogs`; every product has one now); `ship_a_service` and `tune_slow_search` (#663); `respond_to_leaked_secret`, a leak walked end to end (#661); roll code's connect panel offers lockdown, and `claude mcp add` lines quote a
   URL with a query (#659); roll code's answers cut at 48 KiB on both servers (#654, `roll_auth::lockdown::cut`
   in `mcp::call`); invisible characters escaped on `/v1/code/mcp` (#653, `roll_auth::lockdown::visible`); lockdown on roll code's own MCP server (#650: `/v1/code/mcp?lockdown=1`,
   `roll_auth::lockdown` shared with `/v1/mcp`); the sweep's rolld on port 0 with `--addr-file` (#648, after #596); the sweep as a member (#647: a viewer signed in by email code calls every
   member-product read their client lists, none refused by their role; first run 197 listed, 75
   called) and `secrets_list_recoverable` (#585's deleted projects; recovering stays a person's); a member's MCP client reaches what their role does (#644: previews in
   `USER_REACH`, held to the gate's rules by rolld's `auth::tests`; `tools/list` within a member's
   session, so a viewer isn't shown writes and reveals the gate refuses); an outsider's reply never becomes a reviewer learning (#639: on a public repo
   a reader's reply to roll's finding became what roll's reviewer kept in mind for every later
   review once a writer resolved it; the test fails without the fix); roll code's tools say
   whether a retry is safe (`idempotentHint`, #642); `hand_to_agent`, a problem handed to a coding agent and its change brought
   back for review, workflows that may need roll code's tools (`prompts::FRONTED`, held by rolld's
   tests), and `list_agent_threads` answering each machine's projects (#637); roll code's second batch of routes with no tool (#635: AI review jobs,
   prices and spend; a repo's reviewer, review rules and audit log; previews made and removed,
   `stop_preview` destructive; automations on, off, retried, their catalogue and a dry run, retry
   and enable open-world; sandboxes listed, stopped and their use; learnings kept back until they
   carry `outside`, deleting an automation until an agent needs it); project members' roles as `workspace_list_roles`, #501's last route; roll
   code under the tool-or-reason test (`mcp::ROUTES` and `crates/code-control/src/no-tool.txt`,
   method by method as the routers declare them); ten roll code tools: threads resolved, reopened
   and applied, review requests, commit statuses read, a work item read and updated, a thread's
   work given back, a change updated or marked ready, a queued change's lane; `get_change` with
   reviews' text (#631, after an outside review found five issues before it landed); search's browser keys, list and revoke (#625); roll code's routes with no
   tool as tools: `enqueue_change`, `dequeue_change`, `close_change` (destructive), `report_check`,
   `set_commit_status`, `list_refs`, `create_repo` (#624); `postgres_activity` and
   `postgres_health_checks` as fixed SQL, slow queries with their text, and the sweep failing a
   fixed query's SQL error (#618); dropping objects needs `delete` (#611: a write-only grant's
   writer joins `roll_no_drop`, and an event trigger in `roll_guard` refuses its drops; deleting
   rows stays a write, since `DELETE FROM t` empties a table as `TRUNCATE` does); a write-only
   grant's SQL never runs as the superuser (#606, decision 6's option (b): a member of the
   database's `roll_owner`, `WITH INHERIT TRUE, SET FALSE`, never `roll`; option (a), a role with
   no ownership, would have broken every write token's migrations); `what_needs_me` (#597); query
   parameters and body fields held to their routes by tests (#589, #594), apps' S3 and bucket
   drains and `apps_import` (#584, #592), a PagerDuty key redacted outside queues (#586);
   `previews_wait` (#555), 2026-07-28's resource-not-found code (#556), workflows for previews,
   tasks, queues and streams (#560), search mode's ranking (#561), `apps_project_usage` and
   `review_costs` (#564, #565), roll's icon in `server/discover` (#569), named-tool tests (#571),
   `secrets_credential_check` and placeholders (#572, #573), `apps_wait_deploy` and deploy
   approvals (#577). What has no tool goes in `no-tool.txt` with why; a product's new route fails
   roll-mcp's tests until it has a tool or a line.
9. **Landed 2026-11-20, evening (main 32352f2fc, ci green):** secrets' toolset in areas (#538);
   the coming routes of #501 and #520 declared (#540); a second outside review's fixes:
   `secrets_ensure_environment` never deletes keys, `previews_set_rules` leaves GitHub's visual
   checks alone, `secrets_apply_changes` described truly and destructive, code resources refuse
   dot-only names (#542); a test that an area lists only its tools, `debug_failing_service` reads
   grouped errors and asks why a deploy failed (#543); `roll mcp config --toolsets` takes areas and
   refuses an unknown product (#548).
10. **Landed 2026-11-20, fourth turn (main 1f29e2feb, ci green):** roll code files as resources
   (#516); apps' explain and config rotate as tools (#521); every catalogued route a tool's or in
   `no-tool.txt` with why, a test failing on neither (#519); its `no tool yet` lines worked down:
   overviews, replicas, a vhost's suspend and wake, storage and tasks' reads and revokes (#527),
   secrets' environments, folders, imports, required keys, changes, transit keys (#528, with
   `Tool::bodiless` and search mode reading "roll back" as rollback), a project's operational
   preview rules (access, secrets, egress, variables, masking and jobs stay a person's) and apps'
   push status (#531), secrets' change requests without bypass (#534), value requests on #277's
   `rlq_` token and pulled audit keys in `security_checkup` (#536); the destructive hint derived
   for every `DELETE` (#510); secrets' read tools (#506).
11. **Landed 2026-11-20, third turn (main 52220afbe, ci green):** apps' runtime config, deploy
   freeze (admin by the body), drains' new sources and format, and error triage as tools (#495,
   37180c509, after apps' #474 and #480); apps' toolset in areas, `apps.observe`, `.deploy`,
   `.configure` and `.project`, each with the lookups every area needs, placed by route with a
   test for any route it doesn't place (#497); the connect panel offering them (#500).
12. **Landed 2026-11-20, second turn (main 3b1aa5823, ci green):** roll code's `list_repos` and a
   form's repo and owner choices (#462, 573e2691e); apps' and previews' everyday routes as tools
   (#471); taking a status page down (#473); `secrets_audit`, search's overview, unlinking a remote
   and revoking a key (#479); queues' exchanges, bindings and replay, revoking a stream's or an S3
   key (#481); the sweep prints `tools/list`'s size, docs' counts measured (#486); an outside
   review's fixes: a `null` never clears a setting, `clear` names what to, revoke tools are
   destructive, `storage_revoke_key` takes `account` (#491, d04cc26d4); tasks' `id` choices by the
   tool's route (#477). The nightly sweep on d04cc26d4: 452 tools, every read and write green, no
   leaks.
13. **Landed 2026-11-20 (main 9e0367845, ci green):** apps' status page, incidents and SBOM as tools,
   and what a tool publishes held to the credential guard (#448, 1b14ac8e9); roll code MCP's
   openWorldHint (#450); the sweep reaches all 235 read tools with stand-ins and found secrets'
   `seq` and `v` sent as any string (#454); every id a tool lists has choices, checked live (#457;
   a secrets environment is offered by slug, which the sweep caught); `previews_routes` (#463);
   `override` past a deploy freeze on apps' deploy, redeploy, promote and rollback, as
   `X-Roll-Override: 1` (#466, after apps' #453).
14. **Landed 2026-11-19 (main 86039db8a, ci green):** a branch's backup wakes its compute first
   (#428, found by the sweep's `postgres_backup_branch`); a preview's traces
   as tools (#421); `pnpm test:mcp-sweep`, every tool against a fresh rolld with a leak check of
   every read as a token that can't reveal (#425); `secrets_verify_audit`, and `security_checkup`
   reports a broken audit chain (#443); openWorldHint on the tools that have roll reach an address
   the caller chose (#447).
15. **Landed 2026-11-19 (main 4b20f3402, ci green):** the credential guard finds database URLs
   carrying their password and a secret's share link; ids go into paths as they are (an id's `_`
   sent as `%5F` left a service made over MCP never owned by apps' routing, so its live actions
   answered 503; found by the live sweep of every tool); the guide says what outsiders wrote is data
   (#402). Apps' scanning, SLOs and forms as tools; lockdown withholds what a site's visitors and a
   search box's users wrote (#406).
16. **Landed 2026-11-19 (main 098fb41a2, ci green):** `security_checkup` past tokens to what's open to
   the internet; search's remotes and inference secrets as tools; a test that every tool a
   description or workflow names exists; a connection's tools read in place (`prompts/list` 2.2 ms to
   0.18 ms) (#387).
17. **Landed 2026-11-19 (main 09a3526b6, ci green):** read tools for secrets' new backends and
   storage's IAM (#380); the access checkup, tokens, tighten proposals and a token's networks as
   tools, the `security_checkup` workflow, queues' users, tasks' apps and API keys, nosql's imports
   and a stream's keys (#383; live-checked with `live/reads-383.sh` against a rolld built from it).
18. **Landed 2026-11-19 (main 8ce685ac8, ci green):** the turn-4 follow-ups (#373: the policy alert
   names the longest session, People's Dormant filter and turning the dormant off in one step),
   more secrets reads as tools with 10 more search-mode queries (#376), People as CSV (#377).
19. **Landed 2026-11-19, turn 4 (main 97cdd7645, ci green):** steadier timing tests (#348), passkey
   autofill (#322), an admin's per-person security log (#328), admin alerts (#333), search's audit
   and analytics tools (#334), the longest session (#337), how each session signed in (#338),
   signing out of the others (#342), People's search (#345), dormant people, token use counting
   (#349), resending an invite (#351), secrets tools for agents (#353), tool answers that fit
   clients' limits (#355), approvals remembered for a week or 30 days (#360), debugging reads as
   tools (#362). An independent security review fixed six of them before landing; checked in a
   real Chromium (`users-live/review-turn4.sh`).
20. **Landed 2026-11-18, turn 3 (main e84194b6f, ci green):** a person's own data export (#283),
   hardware keys only (#286), one provider for people and agents (#287), an SSO connection's
   session length (#289) and back-channel logout (#292), allowed networks (#294), a second factor
   for everyone (#298) and who has one (#299), an idle timeout (#302), phishing-resistant sign-in
   (#316), moving an account to another address (#306), deleting your own account (#309),
   downloads that send the session plus the sign-in report link (#307), tools for search's
   metrics, slow log and IP allowlist, previews' metrics and secrets' TOTP, SaaS rotators and
   Kubernetes clusters (#310), and the audit log naming people by handle (#313). Gated as one tree,
   then landed one per round (`turn3-land.sh`: the queue drops stacked PRs that share anchors once
   a squash breaks their ancestry).
21. **Landed 2026-11-18 (main 1a60b43be):** passkeys (#256), sudo mode (#257, #260), new-sign-in mail
   (#258), second-factor resets (#263), refused codes in the security log (#254); for roll secrets,
   `revoke_many` and the public detectors (#267) and decoy tokens (#270).
22. **People's accounts, enterprise-grade:** on main: SSO (#225), SCIM provisioning (#231) with
   checksummed `rlp_` tokens revoked on a leak (#250), accounts in roll's audit log (#236), a
   person's own security log (#239), one place for identity providers (#252: an enterprise
   provider with `sso` vouches for its SSO connection's proven domains), and one provider for
   people and agents (#287: "Use for agents too" on an SSO connection). Left, both blocked:
   - SAML for providers without OIDC: its signature checks are s3-server's (`s3-proto`'s xmldsig,
     storage's STS); reuse means a shared crate, so the storage thread agrees first.
   - Provider groups to roles over SCIM: **blocked.** roll has no org or team membership for people
     yet (docs/accounts.md phase 2, "Orgs"), and its one role, admin, is never SCIM's to grant.
     Groups map to org membership once orgs exist.
23. **Gate flakes under load, for their owners:** code-sql's `writes_are_serializable_and_retried`
   and `killed_connections_never_lose_or_repeat_acknowledged_writes` (16 threads on one row exhaust
   the 40 conflict re-runs), and code-control's Linear e2e (30 s for the elicitation). Measured
   2026-11-20 on Postgres 18 at load 40-48: the first took 633-727 runs for its 400 increments
   (about 1.7 attempts a write, the limit 40) and both passed 5 of 5, so the failures were heavier
   load than that and no test was changed (8 threads × 50 would take about 1.3). When one fails
   again, keep its run's "increments took" line and the load: that says whether contention or the
   machine was the cause. A test-side deadline, not more retries in production, stays the fix. (The 2026-11-18 turn's combined
   gate passed without them: #208, #209, #210, #225, #226, #231, #236 and #239 landed as one gated
   tree, main 53b939213.)
24. **Enterprise sign-in (#210), what's left:** a live check with a real provider's ID-JAG (Okta's
   Cross App Access sandbox needs an account: the review pack's decision 5). The draft names the
   tenant `tenant`; a provider whose assertions say it another way (Entra's own tokens use `tid`)
   gets a mapping once its ID-JAGs are checked, never a guess.
25. **Outsiders' text reaching roll's own agents:** checked 2026-11-19. An issue or comment from
   someone GitHub says can't push starts no automation unless its trigger says `anyone`
   (`chatops/github.rs`, as GitHub's lockdown does), forks' code waits for `approve-run`, and only
   an install's members drive roll from Slack or Discord. Left, the agents area's call: when a
   trigger says `anyone`, run its agent with a read-only grant (or ask before every write), since
   the outsider's text is then the agent's task. roll's agents don't connect to `/v1/mcp` (their
   MCP server is the orchestrator's), so `?lockdown` isn't where this belongs.
26. **More MCP Apps views:** a rollback button on a deploy (calling `apps_rollback` through the
   host, so through the gate, as deploy), once a host that renders views is confirmed (the review
   pack's decision 4). A preview's screenshot against its baseline needs no view: hosts render
   `previews_screenshot`'s image (`diff=base`) themselves, and accepting a visual change stays a
   person's call (no tool, as `approve-run`). Check views in ext-apps' reference host
   (`examples/basic-host` v2.0.4: `users-live/host-shot.mjs`, with its one-line patch declaring
   the extension) and the stand-in; Claude.ai or ChatGPT need a public HTTPS rolld and the user's
   account.
27. **More tools that wait:** `apps_wait_deploy`, `apps_wait_job`, `tasks_wait_run` and, since
   2026-11-20, `previews_wait` (until a preview is ready, failed, sleeping, going or waiting on a
   person's `approve-run`). Rechecked on main (05:50): Postgres
   branch backups and restores answer once done (nothing to wait on); search's reindexes are
   Elasticsearch's own `_tasks` behind the operator's request route. Branch backups and search reindexes
   can use `until` once their routes answer a state to wait on (search's reindex is an
   Elasticsearch task today, with no roll route to watch). Recheck with `mcpc` (`~/.local/share/roll-mcp/live/mcpc-tasks.sh`), and with the
   TypeScript SDK's `client/ext/tasks` once it ships.
28. **Dry-merge the sibling heads after each of their pushes** that touches the gate, the catalogue or
   rolld, and send each thread its conflicts and their resolutions (`sibling-auth.sh`: every
   route classified, per method, with #25; agreed conflicts resolved by `known-conflicts.py`), and
   compare each product's routes with its tools (an everyday route with no tool is a gap: apps'
   were found this way). #24 and #25 are on main, so `feature/mcp-on-landing` is retired:
   secrets' #64 lands on main directly.
29. **The operator's steps:** only if a load balancer that terminates HTTP sits in front of rolld (not
   roll's own edge, which needs nothing), set `ROLL_TRUSTED_PROXIES` to its addresses; and register `rlt_`, `rlo_`, `rlf_`, `rlc_`, `rlk_`, `rlp_`, `rlb_` and `rlq_` (`[a-z2-7]{40}` with a
   checksum) with GitHub's secret scanning partner program, so leaks on GitHub reach
   `/v1/access/leaks/github`.
30. **Postgres e2e:** five history/replica e2e tests fail on this server because computes can't
   reach the daemon on the Docker bridge; run `pnpm test:pg` where they can (CI) before merging.
31. **Step-up authorization (SEP-835): moved up to item 2 (2026-11-22), clients do it now.** The
   design as first written: a refused call could answer `403`
   `WWW-Authenticate: Bearer error="insufficient_scope", scope=…` so the client reopens roll's
   consent screen for the refused rule (as the refusal's widen link proposes it now). Not yet: today
   it would take the refusal's explanation and `access_request_access` away from the main clients,
   which don't step up (Claude Code's anthropics/claude-code#44652 was closed as stale on
   2026-06-25, not fixed; Codex's openai/codex#20518 is open, rechecked 2026-11-21; ChatGPT reads `_meta["mcp/www_authenticate"]` on an in-band error). roll's
   unit is a rule chosen at consent, not a scope, so the challenge's scope would name the rule
   (`roll:add:<product>:<action>…`) for the consent screen to propose, never grant by itself.
   Recheck those issues each pass.

Considered and left out: `bypass` on `secrets_rollback` and `secrets_apply_request` (an admin
landing past a protected environment's approvals: a person's, never an agent's); bringing a transit
key's material (`key` on `secrets_create_transit_key` and `secrets_rotate_transit_key`: a
credential in a tool's arguments, where roll makes one instead). Both are lines in
`crates/roll-mcp/src/no-field.txt`: `tools_take_their_routes_body_fields` reads each product's HTTP
contract (apps, workspace, previews, queues, secrets) and fails on a body field or query parameter
the route's tool doesn't take and no line explains, as `no-tool.txt` does for routes. Left out by
it: `follow` (a live stream; a tool answers once), files and exports for people (`format`), a
cache validator (`digest`), and a protected environment's delete confirmation (`confirm`). A docs search tool (Supabase's `search_docs`): roll's docs are design docs (plans, `## Next` lists, test maps), and serving them would hand agents roadmap notes as how roll works; `roll://guide` covers using roll over MCP, until user-facing docs exist; a smaller default toolset (see Toolsets: it would drop tools from connections that use them; search mode is opt-in); a connection narrowed to one project (Supabase's `project_ref`): their
tokens are account-wide; a roll token's rules already name projects, and the builder's presets
start from one.

Done lately: #231 (SCIM provisioning; review: an admin's disable stays, deactivation always finishes, UPN userNames) and #236 (accounts in the audit log; review: events survive dropped connections, what changed, who linked) opened; #225 opened (enterprise SSO for people; an agent's review before its first build found three highs, all fixed: links held to verified domains, no silent re-link, domains proven by DNS); #226 opened (MCP tools for apps' stack and previews' requests); the restore of an apps service is a deploy row of its own (agreed with apps for #230); #114 on main as 93fc949b5; #208, #209, #210 opened; MCP clients' OAuth tokens held to the MCP server (RFC 8707: the API refuses them directly, so lockdown, the credential guard and a read-only URL can't be stepped around; `roll login` keeps API tokens; #210); enterprise sign-in checked against the ID-JAG draft -06 (one audience, a multi-tenant issuer pinned to a tenant, the token held to the assertion's resource, the grant profile in metadata; #210); enterprise apps rotate, change rules, and a leaked app secret is revoked (#210); #179 on main as fc1d44f61 (roll-net refuses 198.18.0.0/15, 192.88.99.0/24, Teredo and IPv4-compatible IPv6, found by previews' egress review); #108 (skills, waits, missing ids asked for, install links) on main as 886fcdd54; products are told which approval let a request run (`roll_auth::Approved {id, by, until}`), so roll secrets' audit can say who approved a reveal instead of building access requests of its own (asked for by the secrets thread; #114); #104 (server card) on main as 22d58038d; the last everyday tasks routes as tools (`tasks_delete_queue`, `tasks_cancel_messages`; batch publishing is `tasks_publish` repeated; #114); everyday routes rechecked after apps #115, search #103 and #127, and previews #107 (what has no tool is a credential, a shell, files or a spend cap: admin by design); "Asked for" names the project and shows the rule on its own line (#114); a refusal links to where a person can allow it (the token page proposes the rule: allow, ask each time, not now; #114); a `set-up-ci` skill (#108); the panel's Rotate moves a client to GitHub Actions, a key or a new secret (#114); #101 (machine clients) on main as fb01574c8; `roll token client rotate --github/--gitlab/--issuer` (#114); one-click installs for Cursor and VS Code on the connect panel, never with a token (#108); everyday routes rechecked after apps #99 and search #100 (what has no tool is interactive, a credential or admin); the checkup nudges machine clients on a secret toward a key or CI's identity, and counts their sign-ins as use (#114); any CI's identity from the CLI (`--issuer --subject --claim --jwks`; #114); every machine client's sign-ins capped, counted once the credential checks out; the list says when each last signed in and what a trusting client trusts; the audit page reads client rows in full (#114); a missing id asked for, checked live with the official TypeScript SDK v2 (#108); a missing id asked for through form elicitation (2026-07-28 clients; #108); secrets' #87 roll-auth lines reviewed and approved (a key-limited rule now passes `filters_itself` rows; health and leaks still show such a token nothing); machine clients in the audit log (`create_client`, `rotate_client`, `revoke_client`, and a `sign_in` row for each token with its credential, CI subject and address; #114); a machine client's token narrowed by `scope=read-only` (#114); deploys and jobs in the MCP Apps view (#108); the machine-client security review's fixes (a device flow can't name a machine client, sign-ins issue at the checked credential generation; #101, #114); #98 (MCP Apps) on main; trusted clients from GitLab CI (`--gitlab group/project`) and in the GitHub Action (`client-id`, reviewed with the secrets thread; #114); `apps_wait_job` (#108); trusted machine clients (CI's own OIDC token, RFC 7523's `jwt-bearer` grant, for any product's rules; roll secrets' verifier moved to `crates/roll-oidc`, reviewed by the secrets thread; `roll token client create --github owner/repo`); #93 (MCP tasks) on main; `apps_wait_deploy` (#108); everyday routes rechecked after secrets #85 and apps' latest: what has no tool is an operator's or admin by design (`visual/accept`, previews' coming `approve`); skills over MCP (SEP-2640: the workflows and the guide as Agent Skills, exact digests; #108); rotating a machine client (`/rotate`, `roll token client rotate`: a new key or secret, the same id; the old one kept a while or stopped with its tokens); an MCP Apps approval card (`access_wait_approval`: what waits, and roll's page to open through the host, never decided from the view) and links opened through the host; the MCP Apps table checked in ext-apps' reference host (its handshake fixed: `protocolVersion`; times shown as UTC, always-empty columns dropped); the server card (SEP-2127: `/v1/mcp/server-card` and `/.well-known/ai-catalog.json`, checked against the extension's schema; #104); a person's own machine clients (acting as them within their reach, waiting for an admin past the policy, theirs alone to see and revoke); a "Machine clients" panel on MCP & access (a pasted PEM or JWK, or a secret shown once with the `curl` that trades it); `roll token client create|list|revoke` (an openssl PEM public key, Ed25519 or P-256, or a JWK; without one, a secret shown once); machine clients (OAuth client credentials: a secret or a key's JWT traded for an hour's token bounded by the client's rules; replays refused; a leaked `rlc_` secret revokes its client); MCP Apps (`ui://roll/table`: tools that answer rows show them as a table in hosts that render apps); the other products' operator checks reviewed: apps' references were the only one a grant got past (fixed in #80); tasks, storage's console, streams and secrets check the grant first or only skip their own keys; tools that wait (`tasks_wait_run`, `access_wait_approval`) wait on as tasks until it's over; MCP tasks checked live with apify's `mcpc` 0.8 (task, approval, result); MCP tasks (2026-07-28's extension: a long call, or one waiting for approval, is a task its owner polls from any node; sealed results, an hour's TTL, 16 per token); `roll_find_tools` ranks by how much of a name the query covers, and a test holds 30 everyday queries to their tool (titles now say what a tool acts on: "Roll back a deployment", "Upload an object to a bucket"); search mode for clients that can't take the list (the connect panel and `roll mcp config` turn it on for VS Code and Cursor; `claude mcp add` lines quote a URL with a mode); credentials read tokens could see are hidden (a project's preview variables, tasks' forwarded headers, agents' share links); `tests/admin_only.rs` (every "needs the admin token" refusal is admin in the catalogue); apps' notifications and log drains and secrets' key workflows as tools; write and deploy tokens no longer pass roll code's admin-only checks (connecting a GitHub repository, landing with `bypass`), shorten a stream's retention (now `delete`), mint agents machine tokens (`admin`) or share threads (`reveal`); a rule can grant one transit key (`{"id": "prj_x", "index": "payments"}`: the per-key rows name it); loosening a NoSQL database's masking policy or read-only flag is `admin` (it was `write`, so a token that could read and write could drop the masking and read raw values); postgres, Redis, storage, queues, streams and tasks tools for their everyday routes; lockdown (`?lockdown=1`: work items from GitHub issues, reviews and review comments whose author can't push, and changes from forks, are `outside`, and their text is withheld from the model); search mode (`?tools=search`: `roll_find_tools`, `roll_run_read` and `roll_run` instead of ~240 tools; a run is the tool it names to every check, the audit log included; the connect panel offers it past a client's limit); a token's "last day" counts only its own requests (its change history and roll's
revocations were counted as asks: "1 asked" on a token that never made a request; seen in the review
pack) (#25); ended OAuth connections and their kept clients are forgotten after the audit log's days
(each `roll login` made one of each, kept forever) (#25); `roll login` checked live against a rolld
set up as a deployment is (`ROLL_CODE_PUBLIC_URL`, over the tailnet; `users-live/login.sh`): over
SSH it shows a code and link, the device page sees it with its network, allowing it signs the CLI in
(0600), `whoami` names the login, the same server's `127.0.0.1` alias is refused naming the address
to use, and logout revokes the grant (#25); the access page's token list has a filter (name, id,
hint, client, person or product; GitHub and Stripe search theirs), and a token pasted into it finds
its own row by its hint without leaving the browser (#25); a scoped token's row shows what its
secret looks like (`rlt_…a1b2`, Stripe's last four), in the dashboard, `roll token list` and the
API, so a token found in a config can be matched to its row (#25); the seventeenth review (#25's
device flow, `roll login`, the change audit and the mails; nothing high), all fixed: past the
wrong-code limit every code answers 429, a right one too (a hit still answered 200, so guessing went
on); `roll login` checks the server's issuer is the address it was pointed at and its endpoints are
on that origin, and never follows a redirect (a crafted `ROLL_API_URL` could have a real token kept
and sent elsewhere); the device mail names the network the device asked from (it named the
approver's, a phished victim's own) and the page warns when the networks differ; only redirect-less
clients use the device grant; a stray callback doesn't end a login and a silent connection doesn't
stall it; the token file is a fresh random file, never through a symlink; a client's own revoke is
audited and a person doesn't see an admin's address; an allowed device never collected has its grant
revoked (#25); a person is mailed at once when a token, MCP client or device is added to their
account (GitHub's "a new personal access token was added": what it may do, from which network, where
to revoke it; a phished device code reaches its victim) (#25); changes to tokens and access are
audited (GitHub's `personal_access_token.create`, Stripe's key history): who made, changed, rotated,
revoked or approved a token, connected a client or set the policy, from one layer on `/v1/access`,
and the audit page shows who made each change (it showed the token changed) (#25); `roll token
whoami` names the credential that runs and where it came from, and each one set that loses to it
(`gh auth status`; exits 1 with none) (#25); `/oauth/device` checked in a browser against a live
rolld, wide and at phone width (`users-live/device-shot.mjs`: the code entry, a wrong code, a
request with its network and age, allowing it and the poll taking tokens, refusing one and the poll
hearing `access_denied`; the wrong-code page said "start connecting again from your MCP client", now
its own words) (#25); `roll login` signs in with a code where there's no browser (RFC 8628's device
flow, `gh auth login`'s: `/oauth/device` on any device, the consent screen's form, the asking
network shown against device-code phishing, ten wrong codes a minute; over SSH by default) (#25);
`apps_set_environment` takes `held` (apps #26's rollback hold) (#24); `apps_export_project` (apps
#26: a project as a roll.toml, variables by name only; pairs with `apps_import`) (#24); `roll login`
and `roll logout` (#25): the CLI signs in through the browser (`gh auth login`'s flow: RFC 7591
registration, a loopback redirect, PKCE, `state` and `iss` checked, `--no-browser` to paste the
answer back), keeps the login 0600 per API URL, uses it only when no credential is set, refreshes it
under a lock, and logout revokes it; revoking a refresh token now revokes its connection (RFC 7009
2.1: a client's sign-out left the grant listed as live with no secrets) (#25); the integration with
secrets' landing branch built as `feature/mcp-on-landing` (#25 + 43cb35162) and every sibling head
route-checked with #25; approving or denying a person's token is audited and keeps who decided
(`decided_by`; GitHub's `personal_access_token.request_approved`), and the token row says "approved
… by …" (#25); a waiting token answers 403 `pending_approval`, not 401 (a 401 sent an OAuth MCP
client back through sign-in and consent for a connection that was only waiting) (#25); a person is
told before making a token or connecting a client that it will wait for an admin (GitHub's "this
token will require approval"; `approve_people` in `GET /v1/access`) (#25); the sixteenth review's
#25 findings fixed: a person's OAuth connection past the policy waits too (it didn't), a waiting
token's rules are frozen until an admin decides (an approval took whatever it had become), widening
and waiting again are one transaction, admins' mail is cleaned names and once an hour per token, and
tokens made before the policy show as `unapproved` in an admin's checkup; the approval of people's
tokens checked live with accounts and mail (`users-live/approval.sh`: waits, 401 `pending_approval`,
the admin mailed, approved from the CLI, the person mailed, then 200); a person's pending token
leads an admin's checkup ("Needs attention": approve or deny), and stays out of the person's own
(#25); admins are mailed when a person's token waits for them (who asked, what it may do, the link
to decide; `Sessions::admins`, accounts' list) (#25); `roll token list --pending` and `roll token
approve|deny <id>` (#25); people's tokens past the policy wait for an admin (#25; GitHub's
organization approval: `approve_people`, 401 `pending_approval`, approve or deny on the access page,
the person mailed, widening waits again); the ninth review's findings (people's mail in roll's words
only: names as words, an approval's mail without its resource, a leak's without where, one approval
mail per token per ten minutes and 20 a person an hour; workload tokens capped per owner and a
person's listed before the limit; a refresh token's retry within its grace audited; apps settings
that change what production runs, its source, build, start and pre-deploy commands, schedule, kind,
port, volume and `run.public`, are deploy; `create_grant` refuses extensions that name no caller;
the expiry digest leaves out tokens made for a day or less); MCP 2026-07-28 checked end to end with
the official TypeScript SDK v2 (`@modelcontextprotocol/client` 2.4.0, `live/sdk2/`): pinned to
2026-07-28, on `auto` (it probes `server/discover` and picks the modern era) and legacy, each
listing tools, calling one, reading a resource and listing prompts; and an approval as a Multi
Round-Trip Request, the SDK's own elicitation handler fulfilling it and retrying the call by itself
(#24); apps reading a roll code repo for the caller (a plan, an import, a service built from it)
needs read on that repo (any apps grant reached it; roll apps' finding) (#24); rolld builds in its
image again (roll-mcp included the dashboard's icon from apps/, which the image leaves out) and the
cluster e2e builds only its own checkout's code (a shared cargo cache id handed one branch's crates
to another; roll previews' finding) (#24); MCP 2026-07-28, the stateless core, beside the handshake
revisions (a dual-era server): `server/discover`, requests served on their own `_meta` with no
`initialize` or session, `MCP-Protocol-Version`/`Mcp-Method`/`Mcp-Name` checked against the body
(`-32020`), unsupported versions answered with the list (`-32022`), unknown methods 404,
`resultType` and `serverInfo` on every result, lists cacheable a minute for the caller only, and
approvals as Multi Round-Trip Requests (`input_required` with the approval page; the retry runs once
approved); a modern-only client failed against roll before (#24); Redis's `@dangerous` commands are
admin (`MONITOR` and `SYNC` stream everyone's commands and data, `CLIENT` ends other connections,
`SLOWLOG` shows others' arguments: they were write) and `SWAPDB` is delete; `run.rollback_on_crash`
is deploy (it puts an older deployment live by itself; apps' review) (#24); an apps service's run
settings fail closed: only size, scale, health checks, timers, alerts, rollout and restarts are
write, every other run key is deploy, a new one too until it's listed (a list of deploy keys let
`run.tcp`, a port on the internet, through as write, as it had `run.schedules`) (#24); setting a
service's `run.schedules` (apps-next: commands run as jobs of the live deployment on a timer) is
deploy, as a production job is (it was write: a token that couldn't run one production job could
schedule one every minute), and `apps_update_service` says how to set it; `secrets_get_values` (one
reveal for an environment, not one per key), `secrets_list_leases` and `secrets_end_lease`, and the
secrets tools' descriptions brought up to date (secrets' seventeenth review) (#24); `secrets_set`,
`secrets_delete` and `secrets_propose` take `if_version` (Vault's check-and-set: an agent can't
silently overwrite a person's edit; 409 `stale`; secrets #62); this round's gate additions cost
nothing measurable (`gate_latency`, release, pinned, eight alternating runs each at load ~44: CPU
2.88 s against 2.96 s, within ±0.25 s run to run; 32-at-once token throughput median ~60k against
~55k, ranges 35–89k and 23–86k overlapping; one resource p50 84–97 µs against 114–133 µs);
`apps_set_environment` takes `paused` and `apps_create_service` a repo's `branch` (apps-next); every
answer to an expiring token says when (`Roll-Token-Expiration`, GitHub's
`GitHub-Authentication-Token-Expiration`), and the CLI warns a week ahead; `apps_import` (a
roll.toml or compose file checked before `roll up --all`; apps-next); the approval UI checked in a
browser against `users-live` (the policy's checkboxes, a waiting token's badge with an admin's
Approve and Deny, "Needs attention" leading with it, the person's view without them), which moved
rotation's choice into a step Rotate opens (it was a select on every row); the `tune_slow_query`
prompt (Neon's query tuning: the plan, an index tried on a branch, both plans compared, main only
when you say so); the sixteenth review (this round's token surface): a rotated secret in its grace
calls products but manages no tokens (it couldn't rotate, change or make one that outlives the
grace), and a token whose checksum is wrong is refused before any lookup; `debug_failing_service`
starts from what roll already flags (an environment's `attention`, `apps_activity` with `all`);
`apps_activity` takes `all` (the whole timeline: deploys, failures, crash loops, alerts; apps-next);
`secrets_reach` and `secrets_folder_policies` (secrets #61, #53); rotating a token can keep the old
secret working a while (`keep_old_for_s`, at most a week: Stripe's roll key), from the dashboard's
Rotate and `roll token rotate --keep-old 1h`, so deployments switch without an outage;
`postgres_typescript_types` (Supabase's generated types, written by one read-only query: enums as
unions, arrays, nullable) and the `compare_branch_schemas` prompt (Neon's schema compare: both
branches described, the difference listed, advisors on the branch); open registration's limit is a
sliding minute per network (a fixed minute let twice the limit through at its edge, and its test
failed when a run crossed one); a token used before its start says so (401 `not_yet_valid`, "this
token works from <time>", with `not_before`; the token list and page show the start); a token can
start later (`not_before`, Cloudflare's TTL start): refused until then as an unknown token, the
dashboard's "Starts" and `roll token create --starts 2h` (the expiry counts from the start),
changeable and clearable; a person's own token's page tested to show only their reach; `roll token
list --product/--resource` and `--workloads`; a workload token makes no tokens; workload tokens
(`Access::mint_workload`: minutes, within an identity's scope, owned by its maker, out of the list
and caps) for secrets' identity logins; a person's own tokens' news is mailed to them (an approval
waiting on them, used from a new network, revoked as leaked, about to expire; only their own, never
a secret); `previews_resolve_note` (a preview page's note, write by the prefix; previews-next), and
`apps_logs` takes `before`, `instance`, `deployment` and `stream` (apps-next); Postgres analysis as
Neon's and Supabase's MCP servers have it: `postgres_explain`, `postgres_slow_queries`,
`postgres_advisors`, `postgres_list_extensions` (read-only SQL through the query route, checked on
Postgres 18); names people choose can't hide text: a token's name with invisible characters is
refused, OAuth clients' names (registered or from a metadata document) drop them, and `initialize`'s
instructions escape them; `apps_set_environment` takes response header rules (`headers`, apps-next);
a cron run's logs are read again, as apps-next decided (its lines were always in the read-level
environment log); `apps_restart_instance` (apps-next, reviewed under the environment prefix: deploy
in production); invisible characters (tag characters, zero-width and bidi controls, variation
selectors) escaped in results' text, so instructions hidden in other people's text stay visible to
the model; plain-read answers checked for credentials (key listings: streams' are usernames,
storage's skip the secret, tasks' are hashes and prefixes); the fifteenth review: a list's
credentials redacted item by item (a token with reveal on one Postgres database and read on all saw
every database's password in `GET /v1/projects`; the same for every list with credential fields), a
preview's `share_url` redacted without reveal (its access token: the QR code's reveal row protected
nothing), and a cron run's logs reveal on this branch too; `previews_list_notes` and
`previews_resolve_note` (a preview page's note, write by the prefix; previews-next), and `apps_logs`
takes `before`, `instance`, `deployment` and `stream` (apps-next); apps-next's cron runs as tools
(`apps_get_cron_run`, `apps_cron_run_logs`, `apps_cancel_cron_run`); a preview's caught mail as
reveal, with `previews_list_mail` and `previews_read_mail` (previews-next); the connect panel says
what a choice of toolsets costs (tools and context tokens, from `GET /v1/mcp/toolsets`) and warns
past a client's limit (VS Code refuses more than 128 tools a request; with every product on roll
lists about 259); the token page counts what a model reads (name, description, schema);
`previews_connect` and its reveal row, and a preview's QR code as reveal (previews-next); the core
products' everyday routes as tools (streams: a topic, a group's lag, branching; tasks: retrying
messages and dead letters, cancelling and rerunning runs, functions, queues; NoSQL: reset, restore,
a branch's diff; storage: branching a bucket, the trash and restoring from it); against Neon's MCP
server and main's routes: `postgres_reset_branch` (reset from parent), `postgres_get_endpoint`,
`postgres_update_endpoint`, `redis_restore`, `apps_delete_environment`; each product's routes
compared with its tools (`tool-gaps.py`): `search_update_cluster`, `search_delete_cluster`,
`secrets_list_rotators`, `secrets_list_syncs` and `secrets_diff` added; previews' bypass, search's
keys, password and console, and secrets' bulk values, reviews and settings stay a person's; apps'
missing everyday tools (`apps_create_service` takes links and run settings, an environment's state,
linking and unlinking resources, checking a domain, volume backups and restoring one; all on main);
a tool any token may call (`apps_plan`) is listed only where its product runs (it showed with apps
off); previews' tools listed (they never were: roll previews runs with roll code and has no product
switch, so MCP's product list left it out; rolld's auth test now fails on any product it serves with
no tools listed); the token builder offers only the products this rolld runs (and any a token's
rules name already), not all sixteen; checked live on #25's build (both official SDKs end to end,
progress during an approval, URL elicitation; `serverInfo.icons` answered), which found roll code's
fronted tools still carrying the approval argument for the operator (37 of 78): they drop it too
now; the fourteenth review (key-scoped rules, before any row uses them): secrets' key names are
literal (a key named `_all` is only that key; a list or a `*` in a request names none) and an
approval remembers only a request that named one key; a pending answer says the call finds its
approval without the argument too; the approval argument only where a person can be asked (a model
reads every tool's schema: 92 → 73 KB for the operator and for tokens that only allow, every product
on; `tools/list` 140 → 120 KB, 190 tools); the builder lists a secrets project's encryption keys to
tick (one picker with search's indices; secrets' projects come from the workspace); encryption keys
as MCP tools (`secrets_transit_encrypt`, `_decrypt`, `_sign`, `_verify`, `_public_key`; secrets #47:
their rows narrow to one key); roll's mark as the server's icon (`serverInfo.icons`); a rule can
narrow a secrets project to some of its encryption keys (`{id, index}`, as search's indices: signing
with one key needn't be write on the project; secrets' transit rows add `.index("name")`);
`secrets_list_rotations` and `secrets_ca_certificates` (secrets #48, #44); `tools/list` 11% smaller
with every product on (183 tools, 150 → 134 KB: the approval argument's description shortened, the
title not repeated in annotations); apps' plan, activity, deploys, links, cron runs and limits as
MCP tools (apps-next #26; the shell, tunnel, hooks and webhook notifications stay out); secrets'
history and workflow as MCP tools (changes, versions, compare, rotate, promote, change requests and
proposing one: how an agent changes a protected environment); `previews_revoke_share` and
`previews_settings` (previews-next #28); `apps_pending` and `apps_deploy_commits` (what the next
deploy changes, what a deploy brought; apps-next #26), and `debug_failing_service` uses them; `roll
token tools <id>` (what an agent sees, from the CLI); roll code's stream and device-login caps take
the client the trusted proxy saw (the right-most `X-Forwarded-For` entry that isn't the proxy, over
every line, only an address), not the left-most a client wrote; apps-next's head passes the
per-method check with #25; the thirteenth review (X-Forwarded-For only from listed proxies, every
line, a bad entry stopping the walk, IPv4-mapped addresses matched; nothing listed, the header
counts for nothing; MCP tool calls carry the gate's address; unconsented client ids good for 90 days
and kept ids small enough for Postgres); `ROLL_TRUSTED_PROXIES` (X-Forwarded-For only from the
edge's addresses, the client the right-most entry that isn't one: a direct request can't name its
own address for IP allowlists or registration's limit); sealed apps variables explained to agents
(apps-next #26); the prefix check per method (rolld logs each route's methods; 194 `METHOD route`
lines reviewed, 7 newly found and looked at: a route with its own row for one method can't gain
another a prefix decides unseen); sensitive secrets explained to agents (`secrets_reveal` and
`roll://guide`: a 403 and `withheld` are expected; secrets #46); open registration stores nothing (a
client's id is the client, signed; kept once a person consents, else good for a day), so a flood has
nothing to fill; the twelfth review's contained findings (CSV cells all quoted with formulas defused
after in-cell separators too; a completion never waits on or uses an open approval; registration's
rate map swept once a minute and bounded; the token-page list read from the caller's address and off
the async threads; stored grants honour the idle policy; client names as consent shows them, no
invisible characters); a person's own token's page tested to show only their reach (#25);
`secrets_list_ssh_cas` (secrets #45); open client registration bounded (20 a minute a network; a
flood never pushes out a client a person is connecting); `secrets_list_cas` (secrets #44); the
prefix check skips public routes (no grant classifies what anyone may call);
`secrets_list_cloud_accounts` (secrets #43); the audit log as CSV (the audit page, `roll mcp audit
--format csv`; formula-safe cells); what an agent sees, on the token page (`GET
/v1/access/tokens/{id}/tools`: the tools and prompts a token is offered and what the list costs an
agent's context); completions named as such in the audit log (`completion/complete:<tool>`); a
connection's tool names worked out once per request; every open sibling's head passes the route
check with #25 (apps-next's rollback to the last deploy, previews-next's share revoke, search's
index refresh and settings reviewed: 178 in all); `secrets_list_aws_users` (secrets #40);
`apps_set_environment` takes who gets in (maintenance, allowed addresses, a rate limit, firewall
rules; apps-next #26); routes a prefix row decides are reviewed (`prefix-reviewed.txt`, 138 looked
at; rolld's auth test fails on a new one until it has its own row or is listed); previews-next's
routes (#28): a preview's env is reveal, accepting a visual change is admin with no tool,
screenshots come back as MCP image content (`previews_screenshot`), `previews_list_shares`,
`previews_env`; the eleventh review (MCP's new surface): read-only excludes reveals that make
something (a presigned PUT, a share link), either switch turns it on and a misspelled key is a 400;
completions offer only ids and never ask a person; prompts' arguments are quoted ids; template
errors say what they were; `secrets_list_outside_databases` (secrets #39); a project's page as a
resource template (`roll://projects/{project}`, read through the gate, its id completed); progress
while a call waits (`notifications/progress` every 10 s for a call with a `progressToken`, checked
with the TypeScript SDK; both SDKs' end-to-end runs pass); `secrets_list_aws_roles` (secrets #38);
read-only connections (`?readonly=1`: only read-only tools listed and run, a checkbox in the connect
panel); `try_migration_on_branch` (a schema change on a Postgres branch first, main only when it
worked and you say so); prompts' arguments complete from what the connection may list
(`completion/complete`, through the gate); MCP prompts (roll's workflows as slash commands: what
this connection may do, checking a diff for secrets, fixing secrets health, debugging a failing
service; offered only with their tools); the integration branch on Postgres 18 (roll-access,
roll-mcp, roll-workspace, roll-secrets) and live (a person's own token over MCP reads their own
secrets project, the operator's answers 404, disabled it stops; `users-live/secrets.sh`); the tenth
review (access with secrets): a person's stored grant dead for products' background work once they
can't sign in (`Sessions::person_now`), hops carrying only their own product's routes,
`Access::revoke_workload`; apps' rollbacks as tools say they put back a deploy's variables
(`keep_variables`), and `apps_rollback_previous` (apps-next #26); every open sibling's routes
classified, built with #25 (apps-next #26, previews-next #28 and search #29 pass rolld's route
test); a preview's share link is reveal (`previews_share`); search's index mapping, settings,
refresh and analyze as MCP tools; `Access::create_grant` (a product makes its own tokens for its
caller through the token route's own function) and tokens listed by what they were made for
(`?product=&resource=`), for secrets retiring `rlst_`; taking an apps web service off the internet
or putting it on (`run.public`) is deploy; a token's rate is the fleet's (each node enforces its
share, the rate over the live nodes), not N times it; a worker's scaling stream needs read on it; a
spent OAuth refresh token used again revokes the connection (OAuth 2.1; 30 seconds of grace for a
retry); only token and access secrets sign a request in.

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
