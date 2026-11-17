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

Every product's tools are about 310 (roll's own and roll code's: ~200 KB, ~25k tokens of what a
model reads), before an agent does anything. Building that list takes 6.5 ms at p50 and 15.5 ms at
p99 in release (`tools_list_cost`, an ignored test: linear in the tools, 3.2 ms for 149), and a
client keeps it for a minute (`ttlMs`). Connect to `/v1/mcp?toolsets=postgres,code` (or send `X-MCP-Toolsets: postgres,code`) to
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
that use them.

### Search mode

With every product on, a node lists about 240 tools (~25k tokens), past VS Code's limit and far
past Cursor's. `/v1/mcp?tools=search` (or `X-MCP-Tools: search`) lists the access tools plus three
tools instead, about 1k tokens whatever runs. This is the pattern of Cloudflare's code mode and
GitHub's dynamic toolsets, applied to a stateless server:

| Tool | What it does |
| --- | --- |
| `roll_find_tools` | Searches the tools this connection may use by what the agent wants to do (`query`, optional `product`, `limit` up to 25) and answers each match's name, description, input schema and which run tool takes it. With no query, it answers every tool's name and title. Words count by how rare they are among the tools, more in a name than a title than a description, and of two names holding the query, the one it covers more of comes first (`apps_deploy` before `apps_cancel_deploy`). `search::finds` holds 30 everyday queries, across every product, to their tool. |
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
`--lockdown` set the rest of the mode). The server itself doesn't switch on `clientInfo`: the
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

### Lockdown

`/v1/mcp?lockdown=1` (or `X-MCP-Lockdown: true`) works like GitHub's MCP server's lockdown mode:
text written by someone who can't push to the repo never reaches the model.

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
// Cursor (~/.cursor/mcp.json), VS Code (.vscode/mcp.json uses "servers"), Windsurf
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
    unknown method is 404 with `-32601`. It's one message, never a batch.
  - `server/discover` answers the versions, capabilities and instructions. Every result has
    `resultType` and `_meta["io.modelcontextprotocol/serverInfo"]`; lists (and discover) have
    `ttlMs: 60000` and `cacheScope: "private"`: a token's tools are its own, and its rules can
    change.
  - Approvals are Multi Round-Trip Requests (see Approvals).
  - **Asking for a missing id** (`roll_mcp::ask`): a call without an id it requires (a project, a
    Postgres project, an apps service or environment), from a client that declared form
    elicitation, answers `input_required` with a form whose choices are what the connection may
    read (the same list tool completions use, through the gate, never asking a person). The
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
| `tune_slow_query` | `project` | A slow query made faster on a branch first (Neon's query tuning): its plan, an index tried on a copy, both plans compared, main only when you say so. | `postgres_get_project`, `postgres_slow_queries`, `postgres_explain`, `postgres_create_branch`, `postgres_execute`, `postgres_delete_branch` |
| `debug_failing_service` | `service`, `env` | Why an environment is failing (what roll already flags in its `attention`, the whole timeline, deploys, deploy logs, failing requests and routes, metrics), then a fix or a rollback it asks about first. | `apps_get_environment`, `apps_activity`, `apps_route_stats`, `apps_list_deploys`, `apps_deploy_logs`, `apps_logs`, `apps_metrics` |

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
| `postgres_update_project` | How long a project keeps history for point-in-time restores and branches. | `PATCH /v1/projects/{project}` |
| `postgres_backup_branch` | Take a base backup of a branch now (restores and new branches start from the newest). | `POST /v1/projects/{project}/branches/{branch}/backup` |
| `postgres_delete_endpoint` | Delete an endpoint (a read replica, or a branch's compute; its data stays on the branch). | `DELETE /v1/endpoints/{endpoint}` |
| `postgres_usage` | Compute and storage use of a project, with cost estimates. | `GET /v1/projects/{project}/usage` |
| `redis_list_databases` | Serverless Redis databases. | `GET /v1/redis/databases` |
| `redis_get_database` | A Redis database's settings and connection (credentials need reveal). | `GET /v1/redis/databases/{database}` |
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
| `storage_update_bucket` | Change a bucket's versioning, lifecycle rules, CORS, trash days, tags or whether it's public: what's left out stays. | `PATCH /v1/storage/buckets/{bucket}` |
| `storage_delete_objects` | Delete these keys, or every object under a prefix (to the trash). | `POST /v1/storage/buckets/{bucket}/delete` |
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
| `streams_update` | Change some of a stream's settings (as streams_get shows them: retention, throughput limits…); the rest stay. | `PATCH /v1/streams/{stream}` |
| `streams_update_topic` | Grow a topic's partitions or change its configs, online. | `PATCH /v1/streams/{stream}/topics/{topic}` |
| `streams_delete_group` | Delete a group and its committed offsets (refused while it has members). | `DELETE /v1/streams/{stream}/groups/{group}` |
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
| `nosql_update_database` | Change a database's name, idle timeout, durability, history, expiry, read-only flag or masking policy: what's left out stays. Loosening masking or read-only needs admin. | `PATCH /v1/nosql/databases/{database}` |
| `nosql_activity` | What a database is doing now: connected clients, slow queries, warnings and metrics. | `GET /v1/nosql/databases/{database}/activity` |
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
| `queues_list_connections` | Open client connections: who, from where, and their channels. | `GET /v1/queues/connections` |
| `queues_close_connection` | Close a client's connection (it may reconnect). | `DELETE /v1/queues/connections/{id}` |
| `queues_list_channels` | Open channels with their prefetch and unacknowledged messages. | `GET /v1/queues/channels` |
| `queues_list_policies` | Policies (message TTL, dead-lettering, length limits) and the queues they match. | `GET /v1/queues/policies` |
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
| `secrets_reveal` | Read a secret's value. | `GET /v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value` |
| `secrets_set` | Create or change a secret's value. | `PUT /v1/secrets/projects/{project}/environments/{env}/secrets/{name}` |
| `secrets_delete` | Delete a secret. | `DELETE /v1/secrets/projects/{project}/environments/{env}/secrets/{name}` |
| `secrets_list_aws_roles` | A project's registered AWS roles for dynamic AWS keys (registering is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/aws-roles` |
| `secrets_list_outside_databases` | Servers roll doesn't run (Postgres, MySQL, SQL Server, ClickHouse, RabbitMQ, Redis, MongoDB) whose users a project rotates (never the admin URL; adding one is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/outside-databases` |
| `secrets_list_aws_users` | A project's AWS IAM users whose keys roll rotates (never a key; adding one is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/aws-users` |
| `secrets_list_cloud_accounts` | A project's GCP and Azure accounts for dynamic credentials (no secrets; adding one is admin, in the dashboard). | `GET /v1/secrets/projects/{project}/cloud-accounts` |
| `secrets_list_cas` | A project's own certificate authorities (public certificates, domains, limits; never a key; adding one is admin). | `GET /v1/secrets/projects/{project}/cas` |
| `secrets_list_ssh_cas` | A project's SSH CAs (public keys, principals, limits; never a private key; adding one is admin). | `GET /v1/secrets/projects/{project}/ssh-cas` |
| `secrets_changes` | What changed in an environment (who, when, which keys; never values). | `GET …/environments/{env}/changes` |
| `secrets_versions` | A secret's history (fingerprints, never values). | `GET …/environments/{env}/secrets/{name}/versions` |
| `secrets_compare` | Which keys differ between two environments (never values). | `GET /v1/secrets/projects/{project}/compare` |
| `secrets_rotate` | A new value for a secret, switched to: a generated one, or a managed link's, made where it lives. | `POST …/environments/{env}/secrets/{name}/rotate` |
| `secrets_promote` | Copy another environment's secrets in (a change request into a protected one). | `POST …/environments/{env}/promote` |
| `secrets_list_requests` | An environment's change requests. | `GET …/environments/{env}/requests` |
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
| `secrets_list_leases` | A project's live leases on dynamic credentials, never a credential. | `GET /v1/secrets/projects/{project}/leases` |
| `secrets_end_lease` | End a dynamic credential's lease now (one's own with reveal). | `DELETE /v1/secrets/projects/{project}/leases/{id}` |
| `secrets_get_values` | All of an environment's values at once, resolved, as one reveal (`dynamic=false` leaves dynamic keys out). | `GET /v1/secrets/projects/{project}/environments/{env}/values` |
| `secrets_health` | What needs fixing in a project's secrets, by name, in the environments you may read: expired, missing, exposed, weak, expiring, stale, unread, rotations or revocations failing, certificates expiring, public (`code`: keys no repository mentions). | `GET /v1/secrets/projects/{project}/health` |
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
| `apps_rollout` | Move a manual gradual rollout that waits: advance, all, or abort (deploy). | `POST /v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollout` |
| `apps_sleep` | Stop an environment's instances until woken. | `POST /v1/apps/services/{service}/environments/{env}/sleep` |
| `apps_wake` | Start a sleeping environment's instances. | `POST /v1/apps/services/{service}/environments/{env}/wake` |
| `apps_usage` | An environment's usage and cost per day. | `GET /v1/apps/services/{service}/environments/{env}/usage` |
| `apps_restart` | Restart an environment's instances. | `POST /v1/apps/services/{service}/environments/{env}/restart` |
| `apps_purge_cache` | Drop a web service's cached answers: some paths, answers with some cache tags, or everything (production: deploy). | `POST /v1/apps/services/{service}/environments/{env}/cache/purge` |
| `apps_restart_instance` | Replace one instance, a fresh one answering first (production: deploy). | `POST /v1/apps/services/{id}/environments/{env}/instances/{name}/restart` |
| `apps_redeploy` | Deploy what's running again with the current settings and variables. | `POST /v1/apps/services/{service}/environments/{env}/redeploy` |
| `apps_run_job` | Run a one-off command in an environment's image with its variables. | `POST /v1/apps/services/{service}/environments/{env}/jobs` |
| `apps_job_logs` | A job's output. | `GET /v1/apps/services/{service}/environments/{env}/jobs/{job}/logs` |
| `apps_list_jobs` | An environment's one-off jobs, newest first. | `GET /v1/apps/services/{service}/environments/{env}/jobs` |
| `apps_get_job` | One job: status, exit code, timing. | `GET /v1/apps/services/{service}/environments/{env}/jobs/{job}` |
| `apps_wait_job` | A job once it ends: a task that waits as long as it takes for a client that runs tasks, its status now for any other. | `GET /v1/apps/services/{service}/environments/{env}/jobs/{job}` |
| `apps_cancel_job` | Stop a running job (production: deploy). | `POST /v1/apps/services/{service}/environments/{env}/jobs/{job}/cancel` |
| `apps_events` | What happened to services, in order after a cursor. | `GET /v1/apps/events` |
| `apps_list_variables` | An environment's variables, values masked. | `GET /v1/apps/services/{service}/environments/{env}/variables` |
| `apps_set_variables` | Set or unset an environment's variables (production is a "deploy" action). | `PATCH /v1/apps/services/{service}/environments/{env}/variables` |
| `apps_metrics` | CPU, memory and requests of an environment. | `GET /v1/apps/services/{service}/environments/{env}/metrics` |
| `apps_plan` | How roll would build and run a repo, before making a service (a dry run). | `POST /v1/apps/plan` |
| `apps_vitals` | What visitors' browsers measured on an environment's pages: Web Vitals p75s, rated, overall and per route. | `GET /v1/apps/services/{service}/environments/{env}/vitals` |
| `apps_route_stats` | An environment's routes over the last minutes: requests, errors, p50/p95/max latency (apps-next). | `GET /v1/apps/services/{id}/environments/{env}/routes` |
| `apps_import` | What a roll.toml or compose file would make, making nothing (apps-next). | `POST /v1/apps/import` |
| `apps_export_project` | A project as a roll.toml, variables by name only (apps #26). | `POST /v1/apps/projects/{project}/export` |
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
| `apps_wait_deploy` | A deploy once it's live or failed: a task that waits as long as it takes for a client that runs tasks, its status now for any other. | `GET …/environments/{env}/deploys/{deploy}` |
| `apps_list_links` | What a service is linked to. | `GET /v1/apps/services/{service}/links` |
| `apps_list_cron_runs` | A cron service's runs. | `GET …/environments/{env}/runs` |
| `apps_get_cron_run` | One cron run: status, exit code, times. | `GET /v1/apps/services/{id}/environments/{env}/runs/{run}` |
| `apps_cron_run_logs` | A cron run's output (read: its lines are in the environment's log). | `GET /v1/apps/services/{id}/environments/{env}/runs/{run}/logs` |
| `apps_cancel_cron_run` | Stop a running cron run (production: deploy). | `POST /v1/apps/services/{id}/environments/{env}/runs/{run}/cancel` |
| `apps_run_cron` | Run a cron job now (deploy in production). | `POST …/environments/{env}/runs` |
| `apps_limits` | A project's apps spend limits. | `GET /v1/apps/projects/{project}/limits` |
| `apps_list_project_services` | The services in one workspace project. | `GET /v1/apps/projects/{project}/services` |
| `apps_list_notifications` | Where a project's deploy, crash and preview events are sent (webhook, Slack, Discord, email; the URL's host only). | `GET /v1/apps/projects/{project}/notifications` |
| `apps_add_notification` | Notify a webhook, Slack, Discord or an email address of a project's events (the URL is stored sealed and never shown again). | `POST /v1/apps/projects/{project}/notifications` |
| `apps_update_notification` | Change which events, environments and services a notification gets: given ones replace. | `PATCH /v1/apps/projects/{project}/notifications/{nid}` |
| `apps_delete_notification` | Stop sending a project's events there. | `DELETE /v1/apps/projects/{project}/notifications/{nid}` |
| `apps_test_notification` | Send a test event now (not retried): whether it arrived. | `POST /v1/apps/projects/{project}/notifications/{nid}/test` |
| `apps_notification_deliveries` | A notification's last 50 deliveries, newest first: what was sent and whether it arrived. | `GET /v1/apps/projects/{project}/notifications/{nid}/deliveries` |
| `apps_list_drains` | Where a project's logs are sent (the URL's host only). | `GET /v1/apps/projects/{project}/drains` |
| `apps_add_drain` | Send every matching service's logs out of roll to an HTTPS endpoint, signed (its signing secret comes back once). | `POST /v1/apps/projects/{project}/drains` |
| `apps_update_drain` | Change what a drain sends or its headers: given ones replace (a new URL is a new drain). | `PATCH /v1/apps/projects/{project}/drains/{did}` |
| `apps_delete_drain` | Stop sending a project's logs there. | `DELETE /v1/apps/projects/{project}/drains/{did}` |
| `apps_test_drain` | Send one signed test line now (not retried): whether it arrived. | `POST /v1/apps/projects/{project}/drains/{did}/test` |
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
| `previews_requests` | A preview's latest 200 requests through roll (service, method, path, status, time), to find the one that failed. | `GET /v1/previews/{id}/requests` |
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

1. **The merge queue, in turn:** #108 (886fcdd54) and #179 (fc1d44f61, roll-net's private ranges)
   are on main. Now previews' security fixes #116/#184/#175, then search's seven, then #114 (main
   merged in at 95600d4fe); then the branches stacked on #114 (two-person approvals, access
   requests, the credential guard, `subscriptions/listen`; the Terraform resource) open as PRs.
   - **Gate flakes under load, for their owners:** code-sql's `writes_are_serializable_and_retried`
     and `killed_connections_never_lose_or_repeat_acknowledged_writes` (16 threads on one row
     exhaust the 40 conflict re-runs), and code-control's Linear e2e (30 s for the elicitation).
     They fail other threads' gates too; a test-side deadline, not more retries in production, is
     the likely fix.
2. **Two-person approvals and access requests: built** on `feature/roll-mcp-approval-quorum` (on
   #114; a PR on main once #114 merges; body in `pr-quorum-body.md`).
   - The policy's `approval_rules` (`{product, actions, environments, approvals, approvers}`, named
     as roll secrets' protected environments) are gates: each approver counts once, never the
     token's own person; with no approvers named only signed-in people count (the operator's
     token and chat ids only where a rule names them); named approvers who aren't admins see and
     decide; pending until every gate is met; never remembered; a remembered rule or an approval
     from before a gate asks again. A security review (agent) found two highs (self-approval from
     chat, one person counting twice under two names) and smaller ones: all fixed and tested.
   - Access requests: a token asks for one rule for a while with a reason (`POST
     /v1/access/requests`, `access_request_access`; Teleport's and Entra PIM's model); approved,
     it's a temporary rule. A refused MCP call tells the agent it can ask, arguments filled in.
   - Chat approvers linked to their accounts (`people`): a press counts for a gate as that person.
   - Reviewed twice by an agent (two highs and smaller issues each time, all fixed); screenshot 42;
     `roll token request --wait`; approval cards name what they're about.
   - Tool lists that change: the call that hears an approved access request says so
     (`notifications/tools/list_changed` first on its stream), and `subscriptions/listen` (2026-07-28)
     tells a subscribed client whenever the connection's tools change; checked live with the
     official TypeScript SDK v2 (two interop bugs found and fixed).
3. **Credentials kept out of what agents write: built** on the same branch (GitHub MCP's secret
   protection; `roll_mcp::guard`), with the values of roll secrets a token may reveal (roll
   secrets' own scan, as the token; agreed with the secrets thread: `X-Roll-Guard: 1`, audited as `secrets.guard` by their #169). roll code's
   tools on `/v1/mcp` are covered (fronted with `roll/product` and `readOnlyHint`); next, the same
   guard on roll code's own endpoint served by `code-control/src/mcp.rs`, with the code thread.
4. **`roll_machine_client` in the Terraform provider:** built on `feature/roll-mcp-terraform-clients`
   (6bc1979b9, on #114; with the secrets thread for review). `TF_ACC=1 go test` is green against the fake
   rolld, and so is a run against a real rolld (`live/tf-machine-client.sh`: create with GitHub's
   identity, rules changed in place, a new subject rotates with the same client id, import by id,
   destroy revokes, a scoped token refused). It opens as a PR on main once #114 merges.
5. **More MCP Apps views:**
   - a preview's screenshot against its baseline;
   - a rollback button on a deploy (calling `apps_rollback` through the host, so through the gate,
     as deploy), once a host that renders views is confirmed (the review pack's decision 4): a
     deploy already shows as its fields and steps (#108).

   Check each in ext-apps' reference host (`examples/basic-host` at v2.0.4, run locally:
   `users-live/host-shot.mjs`; it doesn't declare the extension, so a one-line local patch declares
   it, as a compliant host does) and the stand-in. The table passed there, after a fix: the view's
   `ui/initialize` lacked `protocolVersion`, which the SDK's AppBridge requires. Claude.ai or
   ChatGPT need a public HTTPS rolld and the user's account (the review pack's decisions);
   MCPJam's inspector (3.18) asks for an MCPJam account first.

6. **Enterprise-managed authorization (SEP-990): built** on `feature/roll-mcp-enterprise` (on #114;
   a PR on main after #114). A company's identity provider vouches for a person to an app (the
   IETF's ID-JAG, Okta's Cross App Access): the operator trusts providers (issuer, keys, the email
   domains each vouches for, whether it may sign admins in) and registers apps (a secret or key,
   rules); the app's `jwt-bearer` sign-in with an `oauth-id-jag+jwt` assertion gets an hour's token
   on the person's own connection. Reviewed by an agent (one high: an unverified or any-domain email
   could take an account, admins included; fixed with verified emails in the provider's domains and
   admins only by opt-in; and mediums: read-only scope, Unicode look-alikes, revoked connections
   coming back; all fixed). `roll token enterprise`. Next on it: the dashboard's panel, and a live
   check with a real provider's ID-JAG (Okta's Cross App Access sandbox needs an account: the
   user's call).
7. **Everyday routes with no tool:** tasks, streams, storage, queues, postgres, Redis and NoSQL have
   theirs (`tool-gaps.py origin/main <product>` lists what's left). What's left is mostly what only
   an operator does by hand: keys, passwords, accounts, IAM, roles, CDN, migrations, evict and
   unload, which stay without tools. Recheck each product after its thread lands new routes.
8. **Lockdown by default where roll's own agents act on outside text:** roll code agents and
   automations that pick up issues and chat messages (the agents thread's call; tell it), and
   chat messages that reach an agent marked `outside` (an install's non-members can't drive
   roll, so mostly members' text).
9. **`apps_purge_cache`** once apps' #139 is on main: a sibling of `apps_restart` for
   `POST …/environments/{env}/cache/purge {paths?, tags?}` (tags from apps' #183) (deploy in production, write elsewhere, by
   the environment's prefix row). Likewise `secrets_totp` once secrets' #194 is on main: `GET
   …/environments/{env}/secrets/{key}/totp` (`{code, expires_at, period}`), with the reveal tools
   (its row is reveal, audited as `secrets.totp`); its description says a code lives a period, so
   fetch again past `expires_at` (an approval that waited may hand back one about to expire).
10. **More tools that wait:** `apps_wait_deploy` is in #108. Rechecked on main (05:50): Postgres
   branch backups and restores answer once done (nothing to wait on); search's reindexes are
   Elasticsearch's own `_tasks` behind the operator's request route. Branch backups and search reindexes
   can use `until` once their routes answer a state to wait on (search's reindex is an
   Elasticsearch task today, with no roll route to watch). Recheck with `mcpc` (`~/.local/share/roll-mcp/live/mcpc-tasks.sh`), and with the
   TypeScript SDK's `client/ext/tasks` once it ships.
11. **Dry-merge the sibling heads after each of their pushes** that touches the gate, the catalogue or
   rolld, and send each thread its conflicts and their resolutions (`sibling-auth.sh`: every
   route classified, per method, with #25; agreed conflicts resolved by `known-conflicts.py`), and
   compare each product's routes with its tools (an everyday route with no tool is a gap: apps'
   were found this way). #24 and #25 are on main, so `feature/mcp-on-landing` is retired:
   secrets' #64 lands on main directly.
12. **The operator's steps:** only if a load balancer that terminates HTTP sits in front of rolld (not
   roll's own edge, which needs nothing), set `ROLL_TRUSTED_PROXIES` to its addresses; and register `rlt_`, `rlo_`, `rlf_`, `rlc_` and `rlk_` (`[a-z2-7]{40}` with a
   checksum) with GitHub's secret scanning partner program, so leaks on GitHub reach
   `/v1/access/leaks/github`.
13. **Postgres e2e:** five history/replica e2e tests fail on this server because computes can't
   reach the daemon on the Docker bridge; run `pnpm test:pg` where they can (CI) before merging.
14. **Step-up authorization (SEP-835), once clients do it:** a refused call could answer `403`
   `WWW-Authenticate: Bearer error="insufficient_scope", scope=…` so the client reopens roll's
   consent screen for the refused rule (as the refusal's widen link proposes it now). Not yet: today
   it would take the refusal's explanation and `access_request_access` away from the main clients,
   which don't step up (Claude Code's 403 path is open as anthropics/claude-code#44652, Codex's as
   openai/codex#20518; ChatGPT reads `_meta["mcp/www_authenticate"]` on an in-band error). roll's
   unit is a rule chosen at consent, not a scope, so the challenge's scope would name the rule
   (`roll:add:<product>:<action>…`) for the consent screen to propose, never grant by itself.
   Recheck those issues each pass.

Considered and left out: a docs search tool (Supabase's `search_docs`): roll's docs are design docs (plans, `## Next` lists, test maps), and serving them would hand agents roadmap notes as how roll works; `roll://guide` covers using roll over MCP, until user-facing docs exist; a smaller default toolset (see Toolsets: it would drop tools from connections that use them; search mode is opt-in); a connection narrowed to one project (Supabase's `project_ref`): their
tokens are account-wide; a roll token's rules already name projects, and the builder's presets
start from one. `outputSchema` per tool: it would double `tools/list` for clients that read
`structuredContent` as JSON anyway.

Done lately: #179 on main as fc1d44f61 (roll-net refuses 198.18.0.0/15, 192.88.99.0/24, Teredo and IPv4-compatible IPv6, found by previews' egress review); #108 (skills, waits, missing ids asked for, install links) on main as 886fcdd54; products are told which approval let a request run (`roll_auth::Approved {id, by, until}`), so roll secrets' audit can say who approved a reveal instead of building access requests of its own (asked for by the secrets thread; #114); #104 (server card) on main as 22d58038d; the last everyday tasks routes as tools (`tasks_delete_queue`, `tasks_cancel_messages`; batch publishing is `tasks_publish` repeated; #114); everyday routes rechecked after apps #115, search #103 and #127, and previews #107 (what has no tool is a credential, a shell, files or a spend cap: admin by design); "Asked for" names the project and shows the rule on its own line (#114); a refusal links to where a person can allow it (the token page proposes the rule: allow, ask each time, not now; #114); a `set-up-ci` skill (#108); the panel's Rotate moves a client to GitHub Actions, a key or a new secret (#114); #101 (machine clients) on main as fb01574c8; `roll token client rotate --github/--gitlab/--issuer` (#114); one-click installs for Cursor and VS Code on the connect panel, never with a token (#108); everyday routes rechecked after apps #99 and search #100 (what has no tool is interactive, a credential or admin); the checkup nudges machine clients on a secret toward a key or CI's identity, and counts their sign-ins as use (#114); any CI's identity from the CLI (`--issuer --subject --claim --jwks`; #114); every machine client's sign-ins capped, counted once the credential checks out; the list says when each last signed in and what a trusting client trusts; the audit page reads client rows in full (#114); a missing id asked for, checked live with the official TypeScript SDK v2 (#108); a missing id asked for through form elicitation (2026-07-28 clients; #108); secrets' #87 roll-auth lines reviewed and approved (a key-limited rule now passes `filters_itself` rows; health and leaks still show such a token nothing); machine clients in the audit log (`create_client`, `rotate_client`, `revoke_client`, and a `sign_in` row for each token with its credential, CI subject and address; #114); a machine client's token narrowed by `scope=read-only` (#114); deploys and jobs in the MCP Apps view (#108); the machine-client security review's fixes (a device flow can't name a machine client, sign-ins issue at the checked credential generation; #101, #114); #98 (MCP Apps) on main; trusted clients from GitLab CI (`--gitlab group/project`) and in the GitHub Action (`client-id`, reviewed with the secrets thread; #114); `apps_wait_job` (#108); trusted machine clients (CI's own OIDC token, RFC 7523's `jwt-bearer` grant, for any product's rules; roll secrets' verifier moved to `crates/roll-oidc`, reviewed by the secrets thread; `roll token client create --github owner/repo`); #93 (MCP tasks) on main; `apps_wait_deploy` (#108); everyday routes rechecked after secrets #85 and apps' latest: what has no tool is an operator's or admin by design (`visual/accept`, previews' coming `approve`); skills over MCP (SEP-2640: the workflows and the guide as Agent Skills, exact digests; #108); rotating a machine client (`/rotate`, `roll token client rotate`: a new key or secret, the same id; the old one kept a while or stopped with its tokens); an MCP Apps approval card (`access_wait_approval`: what waits, and roll's page to open through the host, never decided from the view) and links opened through the host; the MCP Apps table checked in ext-apps' reference host (its handshake fixed: `protocolVersion`; times shown as UTC, always-empty columns dropped); the server card (SEP-2127: `/v1/mcp/server-card` and `/.well-known/ai-catalog.json`, checked against the extension's schema; #104); a person's own machine clients (acting as them within their reach, waiting for an admin past the policy, theirs alone to see and revoke); a "Machine clients" panel on MCP & access (a pasted PEM or JWK, or a secret shown once with the `curl` that trades it); `roll token client create|list|revoke` (an openssl PEM public key, Ed25519 or P-256, or a JWK; without one, a secret shown once); machine clients (OAuth client credentials: a secret or a key's JWT traded for an hour's token bounded by the client's rules; replays refused; a leaked `rlc_` secret revokes its client); MCP Apps (`ui://roll/table`: tools that answer rows show them as a table in hosts that render apps); the other products' operator checks reviewed: apps' references were the only one a grant got past (fixed in #80); tasks, storage's console, streams and secrets check the grant first or only skip their own keys; tools that wait (`tasks_wait_run`, `access_wait_approval`) wait on as tasks until it's over; MCP tasks checked live with apify's `mcpc` 0.8 (task, approval, result); MCP tasks (2026-07-28's extension: a long call, or one waiting for approval, is a task its owner polls from any node; sealed results, an hour's TTL, 16 per token); `roll_find_tools` ranks by how much of a name the query covers, and a test holds 30 everyday queries to their tool (titles now say what a tool acts on: "Roll back a deployment", "Upload an object to a bucket"); search mode for clients that can't take the list (the connect panel and `roll mcp config` turn it on for VS Code and Cursor; `claude mcp add` lines quote a URL with a mode); credentials read tokens could see are hidden (a project's preview variables, tasks' forwarded headers, agents' share links); `tests/admin_only.rs` (every "needs the admin token" refusal is admin in the catalogue); apps' notifications and log drains and secrets' key workflows as tools; write and deploy tokens no longer pass roll code's admin-only checks (connecting a GitHub repository, landing with `bypass`), shorten a stream's retention (now `delete`), mint agents machine tokens (`admin`) or share threads (`reveal`); a rule can grant one transit key (`{"id": "prj_x", "index": "payments"}`: the per-key rows name it); loosening a NoSQL database's masking policy or read-only flag is `admin` (it was `write`, so a token that could read and write could drop the masking and read raw values); postgres, Redis, storage, queues, streams and tasks tools for their everyday routes; lockdown (`?lockdown=1`: work items from GitHub issues, reviews and review comments whose author can't push, and changes from forks, are `outside`, and their text is withheld from the model); search mode (`?tools=search`: `roll_find_tools`, `roll_run_read` and `roll_run` instead of ~240 tools; a run is the tool it names to every check, the audit log included; the connect panel offers it past a client's limit); a token's "last day" counts only its own requests (its change history and roll's
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
