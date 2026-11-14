# Permissions: scoped tokens, approvals and audit

roll's control plane takes one kind of delegated credential: a **scoped token**. The user builds it
by choosing products, resources, actions, an expiry and optionally the addresses it may come from.
An agent, a CI job or an MCP client then holds exactly that much power. The same token works on the
HTTP API, through the `roll` CLI and through the [MCP server](mcp.md). Scopes are enforced by the
API, once, in front of every route, so no surface can do more than another.

- Code: `crates/roll-auth` (the scope model, `roll_auth::scope`, and the gate),
  `crates/roll-access` (the store, the catalogue, the evaluator, `/v1/access`, OAuth), wired in by
  `crates/rolld/src/access.rs`. The MCP server is `crates/roll-mcp` ([mcp.md](mcp.md)).
- Client: `apps/dashboard/src/lib/access-api.ts`. Change it and the [HTTP API](#http-api) together.
- Dashboard: `/dashboard/access` ("MCP & access") and the consent screen at `/oauth/authorize`.
  Each token has a page, `/dashboard/access/tokens/<id>`: what it may do in words (resources by
  name), what approvals allow it for a while and until when, the approvals it asked for, its recent
  requests, and edit, rotate and revoke.
- CLI: `roll token …` and `roll mcp …`.
- Tests: `crates/roll-access/tests/access.rs` (scoped tokens, ask, tokens making tokens, OAuth end
  to end), `crates/roll-mcp/tests/mcp.rs`, and `crates/rolld/tests/auth.rs` (every route a real
  daemon serves is classified, and a full token works where the operator's does).

## What good products do, and what roll takes from them

| product | what's good | what roll takes |
| --- | --- | --- |
| GitHub fine-grained PATs | pick resources (all, or selected repos), then a level per permission (none / read / write); mandatory expiry; an org can require approval | resources first, then a level per product; expiry by default; list results filtered to what the token can see |
| Cloudflare API token builder | policies = permission groups × resources (account, zone, or all); client IP filtering; TTL; templates | rules as permissions × resources; IP allowlist; templates ("presets") |
| Stripe restricted keys | per-resource none / read / write; keys show last used | per-action allow; last used on every token |
| Supabase MCP | `read_only` runs SQL as a read-only Postgres role; `project_ref` pins one project; feature groups | read-only SQL enforced by the database, not by parsing; project pinning; tools listed by what the token can do |
| Neon MCP | branch-based workflows; tools for run SQL, migrations on a branch | branch tools; destructive actions marked as such |
| Vercel / Linear MCP | OAuth 2.1 with dynamic client registration; consent per workspace | the MCP authorization spec end to end, with a consent screen that narrows the grant |
| MCP authorization (2025-06-18, 2025-11-25) | protected resource metadata (RFC 9728), PKCE, resource indicators (RFC 8707), DCR (RFC 7591) | all of it ([mcp.md](mcp.md#authorization)) |

Two things none of them combine and roll does: **ask** (an action that pauses until a person
approves it, the request then running as sent) and one evaluator shared by the API, the CLI and MCP.

## Principals

| credential | prefix | what it is |
| --- | --- | --- |
| operator token | (any) | `ROLL_ADMIN_TOKEN`: everything. The person who runs roll. |
| scoped token | `rlt_` | created by the operator (or a token allowed to manage access) with the builder |
| OAuth access token | `rlo_` | issued to an MCP client after the consent screen; one hour |
| OAuth refresh token | `rlf_` | rotates on every use; 30 days since its last use; a spent one used again revokes the connection |
| product credentials | `rlc_`, `rla_`, keys… | roll code owner tokens, storage IAM, stream and tasks keys: unchanged, checked by their product |

Each grant secret is its prefix, 34 random characters of lowercase base32 and a 6-character
checksum (CRC-32 of the rest), 44 characters in all. Scanners use the checksum, as GitHub's tokens
carry one, to tell a real roll token from look-alike text without asking roll
(`roll_access::checksum_ok`, `find_secrets`). Secrets made before checksums still work; they just
aren't recognised by scanners.

A scoped token and an OAuth connection are both a **grant**: one row with the rules, expiry, IP
allowlist, name and who made it. An OAuth connection is a grant whose secrets are the rotating
access and refresh tokens. Revoking the grant revokes them all at once. A refresh token, once
used, is kept as spent until it would have expired: presented again later than 30 seconds after
its use, someone else has a copy (OAuth 2.1, section 4.3.1), so the connection is revoked, with why,
and the audit log has it. Within those 30 seconds it's only refused (a client retrying after a lost
answer keeps its connection), and the audit log has that too.

Grants are made by the operator: the operator's token, or a signed-in admin (docs/accounts.md:
an admin's session acts as the operator). The grant records who made it (`created_by`: the admin's
`usr_…`, "operator" for the token, or the grant that made it), and approvals record who decided
them. A signed-in user who isn't an admin can't manage grants yet: making tokens for their own
repos, agents and projects is the next step, with this same model (their rules can only cover what
they can reach). A grant never uses a session cookie, and a cookie never carries a grant.

Data planes keep their own credentials (Postgres passwords, Redis users, S3 keys, SASL). A scoped
token never opens a data plane. It can be allowed to **reveal** those credentials, which is then
an explicit action of its own.

## The grant

```ts
type Grant = {
  id: string;            // tok_…
  name: string;          // "ci", "cursor on my laptop"
  kind: "token" | "oauth";
  rules: Rule[];         // at least one; nothing else is allowed
  expires_at: number | null; // unix ms; the builder defaults to 30 days
  ip_allow: string[];    // CIDRs; empty: any address
  created_by: string;    // "operator", or the grant that made it ("tok_…")
  created_at: number;
  last_used_at: number | null;
  last_used_ip: string | null;
  revoked_at: number | null;
  client?: { id: string; name: string; uri?: string }; // OAuth only
  rate_per_minute?: number; // 1–100,000 requests a minute across the fleet (each node a share); absent: no limit
};

type Rule = {
  product: Product | "*";
  resources: "*" | Selector[];      // "*": every resource, including ones made later
  environments?: string[];          // only resources in these environments
  paths?: string[];                 // only resources under these paths (secrets' folders)
  actions: Partial<Record<Action, "allow" | "ask">>;
};

type Selector =
  | { project: string }             // a workspace project: every resource in it, as it is now
  | { id: string; index?: string }; // one resource, by its product's id; search and secrets: only some indices or encryption keys
// A selector has exactly these fields: any other is refused, never dropped.

type Product = "workspace" | "postgres" | "redis" | "storage" | "streams" | "nosql" | "queues"
  | "tasks" | "code" | "agents" | "secrets" | "apps" | "previews" | "search" | "observability"
  | "access";
type Action = "read" | "write" | "delete" | "reveal" | "deploy" | "admin";
```

### Actions

Every route is exactly one action on one product. Actions don't imply each other: `write` doesn't
include `read`, and nothing includes `delete`, `reveal`, `deploy` or `admin`. The builder fills in
the usual combinations, so this stays explicit without being tedious.

| action | means | examples |
| --- | --- | --- |
| `read` | see metadata and data | list projects, read a file, `SELECT` in a read-only transaction, peek messages, search, read logs and metrics |
| `write` | create and change | create a branch, run SQL that writes, set a Redis key, upload an object, publish, open a change |
| `delete` | destroy or make irreversible | drop a database or branch, delete a bucket or object, purge a queue, delete a topic, restore over a branch, delete a secret or a service |
| `reveal` | see a credential or a secret value | connection strings with passwords, reset a password, API keys, storage access keys, secret values |
| `deploy` | change production | land a change on a default branch, deploy, roll back or promote to production, bypass a merge queue |
| `admin` | manage access and settings | IAM users and policies, API keys, stream ACLs, owner settings, webhooks, tokens (product `access`) |

A route whose effect depends on its body is classified from the body. Comments are stripped
first, so they can't hide what runs, and anything that makes or changes credentials or privileges
(`CREATE ROLE`, `GRANT`, `SUPERUSER`, …) is `admin`. The routes are:

- Postgres SQL: `{"read_only": true}` is `read`, and the SQL runs as a role made for that one query
  that holds only `pg_read_all_data`, so Postgres refuses its writes whatever the SQL says (on a
  branch's primary; a read replica refuses read-only SQL). Anything else is `write`.
- Redis commands: commands that only read are `read`; `FLUSHDB`, `FLUSHALL` and scripts (`EVAL`,
  `FCALL`, `FUNCTION`, `SCRIPT`) are `delete`; `ACL`, `CONFIG` and the like are `admin`; the rest
  `write`.
- NoSQL CQL: `SELECT` is `read`, `DROP` and `TRUNCATE` are `delete`, the rest `write`.
- Storage IAM: `Get…`, `List…`, `Simulate…` are `read`, the rest `admin`.
- git pushes (`POST /git/{owner}/{repo}/git-receive-pack`): from the ref updates at the start
  of the body (read without buffering the pack). Each update is one action: moving the repo's
  default branch, directly or through `refs/land/<default>` (which lands onto it), is `deploy`;
  deleting a ref `delete`; anything else `write`. A push needs **every** action its updates take
  (actions don't imply each other, so `delete` and `deploy` don't cover a new branch), and the
  approval and the audit log list the updates. A body that can't be read that far, or is
  compressed, needs `write`, `delete` and `deploy`. Fetches are `read`. git sends the token as a Basic password (any user
  name), which the gate takes like a bearer token; roll code's git server then takes the gate's
  word (`code_git::Vouched`) and records the push as `token:<name>`. An `ask` push is bound to its
  ref updates; push again once it's approved (the same push finds its approval), or wait on it with
  `git -c http.extraHeader="X-Roll-Approval: apr_…" push`.
- An apps shell (its WebSocket, and all of a session: opening, keys, screen, resizing, closing) runs
  any command where the service runs, with its variables: `admin` in every environment, as an
  agent's shell is. Waking a sleeping environment starts compute someone pays for: `write`.
- A service's settings (`PATCH …/services/{id}`) are `deploy` when they change what production
  runs or who reaches it: `source` and `build` (the next landing deploys them), `run.start`,
  `run.pre_deploy` (it runs with production's variables), `run.schedule`, `run.kind`, `run.port`,
  `run.volume`, and `run.public` (a web service on the internet or off it, at once). Its size,
  scale, health checks, alerts and rollout are `write`. Rolling back to a deployment puts back the variables it went live with
  (unless `keep_variables`); it's the environment's act as writing its variables is (`deploy` in
  production, `write` elsewhere), and the answer names them without values.
- A route whose product filters its answer per environment and path itself, asking the caller's
  `Grant` for each finding, can be matched by rules limited to some environments or paths too
  (`Row::filters_itself`): a token for staging would get staging's findings, never production's,
  rather than a 403. No row has it yet: secrets' health and leak scan get it with secrets' `Grant`
  swap, and until then secrets answers every request the gate lets through as the operator. For the
  same reason secrets' cross-project routes (`GET /v1/secrets/search`, `GET /v1/secrets/findings`)
  need a rule for all of secrets until the swap makes them any token's. An approval on such a route
  is approved once (a rule from it would drop the token's environments).
- Linking another resource to an apps service (`PUT …/services/{id}/links/{kind}/{ref}`, or
  `links` in a new service's body) puts its address and password in the service's variables, where
  the service's code reads them: it also needs `reveal` on each linked resource (a `postgres` link
  on that Postgres project, `redis` on that database, `bucket`, `stream`, `queue`, `nosql` likewise;
  a kind roll doesn't know is refused). A worker scaled on a stream group's lag (`run.scale.stream`
  in a new service or a `PATCH`) has apps read that stream for it: `read` on it too. When that decides, the approval is about the linked
  resource, so remembering it allows revealing that one (all of reveal on it, not only the link);
  when several linked resources aren't allowed outright, the approval names them all and is
  approved once. Each linked resource's reveal is its own audit row.
- Comparing a local copy with a secrets folder (`POST …/environments/{env}/diff`, by fingerprint,
  names only) tests a guess at a value: `reveal` there, as reading the values is.
- A secrets project's backup (every value, sealed to a key the caller chooses) and restore are
  `admin`, as in secrets itself.
- Secrets' imports (`PUT …/environments/{env}/imports`, `[{project?, environment, path}]`) are a
  copy from each in-project source: `write` on the importing folder plus `reveal` on every folder
  imported. Importing another project's folder is `admin` on the importing folder (that project's
  admin must export it too). Secrets also needs `admin` to change a protected environment's imports.
- An apps deploy hook (`POST`/`DELETE …/environments/{env}/hook`) is a credential that deploys
  outside roll's tokens (its URL is the secret): making, rotating or removing one is `admin`, as an
  API key is.
- Sending a project's data out of roll with a signing secret shown once is `reveal`: an apps
  webhook notification target, and an apps log drain (every service's output, POSTed signed).
- Copying another environment's values is reading them: a row with `copies_from` (apps' `PUT
  …/environments/{env}` and its `variables_from`, secrets' `POST …/environments/{env}/promote` and
  its `?from=`) also needs `reveal` on the same resource in the environment the body or query
  names, and the request gets the weaker of the two decisions. When the copy decides, the approval
  is about the environment and folder copied from, so remembering it allows exactly that. A query
  parameter the gate decides from (`?path=`, `?from=`) must appear once: a repeated one is
  `400 repeated_parameter`, since the product could read another copy of it.
- Agents' machine commands, by `policy::agent_may`'s tiers: diffs and file reads `read`; working
  threads `write`; publishing with `land` `deploy`; a shell, writing files, rolling back, raising a
  thread's runtime or interaction mode, answering an agent's approval, endpoints, the gateway,
  projects, the machine's settings, and any type it doesn't know `admin`.

A public route (`Access::Public` in rolld's `auth.rs`: health, a shared link, OAuth's endpoints)
stays public for a request carrying a grant: it's served as anyone's, with no grant and no
operator marker.

Routes not in the catalogue are the operator's alone. A product that adds a route gets nothing
for scoped tokens until its row exists, and `crates/rolld/tests/auth.rs` fails until every route
rolld serves has a row (or an explicit "operator only").

### Resources

A request's resource is found from its route: a path parameter (`/v1/projects/{id}` is the
Postgres project `id`), or a lookup for ids that don't name their resource (a code change `chg_…`
is in a repo; a Postgres branch is in a project). The evaluator then knows the resource's
**product**, **id**, **workspace project** (from `crates/roll-workspace`, as it is at that moment)
and **environment** where the product says (a secrets environment, an app environment, a preview).

- `"*"` matches every resource of the product.
- `{project}` matches resources in that workspace project at the time of the request. Moving a
  resource out of the project takes it out of the token's reach at once.
- `{id}` matches one resource.
- `environments` narrows a rule to resources whose environment the product knows and is in the
  list. A resource without a known environment never matches such a rule. Secrets and apps carry
  the environment in their paths (`…/environments/{env}/…`); previews' resources are in `preview`.
  An entry ending in `*` is a prefix (`staging-*` covers `staging-eu`); `*` alone isn't one (leave
  `environments` out for all of them). A product can say what an environment named in a path is
  (`Resolver::environment`, registered per product in `Resolvers::environments`): a preview's own
  environment (`pv-12`) is `preview`, so "Previews only" reaches it.
- `paths` narrows a rule to resources with a path under one of these (`/api` covers `/api/db`, not
  `/apix`). Secrets' folders come from `?path=` (`/` without it).
- Ids are canonical: a Redis database, NoSQL database or stream named by its name is checked under
  its id, a Postgres endpoint under its project, a Redis replica under its database, and a code
  change, thread or work item under its repo (`owner/name`). tasks' queues, topics, schedules and
  functions are `queue:<name>`, `topic:<name>`, `schedule:<id>` and `function:<id>`, so a rule for
  a queue never matches a topic of the same name.

Moving a resource into or out of a workspace project changes which tokens reach it, so the
workspace's membership routes (`PUT`/`DELETE …/projects/{id}/resources/{kind}/{ref}`) need
`write` on the project **and** `admin`, allowed outright, on the resource where it is now.

Routes that aren't about one resource:

- **Lists** (`GET /v1/projects`) need the action on the product. Unless a rule allows the action
  outright on every resource, the answer is filtered to the items allowed outright: a token for
  one project sees only that project's databases, and items that would only `ask` are left out
  (a list is never held for a person).
- **Creates** (`POST /v1/projects`) need the action on the product with `"*"` or a `{project}`
  selector. With a `{project}` selector the request must name the project (`X-Roll-Project:
  prj_…`); the new resource is put in it, in the same request, before the answer returns. A token
  for one project can make databases only in that project.
- **Product-wide** routes (settings, usage of the whole product) need a rule with `"*"`.

### Credentials in answers

Many answers carry credentials besides what they're about: every Postgres project view has
connection strings with the password, Redis and NoSQL views have passwords and tokens, bucket views
have webhook secrets. Such rows name their credential fields, and unless the grant may `reveal` on
that resource, the gate replaces them in the answer before it leaves: a URL keeps everything but its
password (`postgres://roll:redacted@host/main`), anything else becomes `"[redacted]"`. A token that
may create a database without `reveal` gets the new database, not its password. `ask` for `reveal`
counts as not allowed here: an answer is never held for a person.

A list is judged item by item: a token with `reveal` on one database and `read` on every one lists
them all, and sees only that one's credentials (the others' are replaced). A preview's links
(`share_url`, in `GET /v1/previews` and `GET /v1/previews/{id}`) carry its access token, so they
are credentials too, as its QR code is.

### Storage accounts

Storage acts in the account a request names (`X-Roll-Account` or `?account=`), and bucket names
repeat across accounts:

- A bucket in a named account is `<account>/<bucket>`, so a rule
  `{product: "storage", resources: [{id: "acme/logs"}]}` reaches `logs` in account `acme` only.
  A plain bucket name (`{id: "logs"}`) means the default account's bucket.
- A named account's bucket list is filtered to the `<account>/<bucket>` ids a token has there.
- Anything else in a named account (keys, roles, IAM) is about storage as a whole: only a rule for
  every storage resource covers it.

### Search indices

A rule can grant some indices of a search cluster: `{id: "cl_1", index: "logs-*,metrics"}`
(the builder writes it `cl_1/logs-*,metrics`).

- It covers the index routes (`/v1/search/clusters/{id}/indices/{index}/…`: search, documents,
  bulk, mappings, the index itself) when every name or pattern in `{index}` is covered: a name
  listed, or one starting with what's before a listed pattern's trailing `*` (`logs-*` covers
  `logs-2025` and `logs-2025-*`, not `logs` or `l*`). Exclusions (`-logs-old`) only narrow, so they
  needn't be covered; `_all` is `*`; date math only by `*`; another cluster's indices
  (`remote:logs`) never.
- The cluster's own routes stay the cluster's: the index list, the console (`request`, any method
  and path as the cluster's owner), keys, branches and settings need a rule for the whole cluster.
- Search checks the rest at query time as the caller: a query's reads of other indices (terms
  lookups, `more_like_this`, lookup runtime fields, `indexed_shape`, templates after rendering, and
  the bodies of reindex, ES|QL, SQL and the other query-bearing APIs) and a body's `_index` values
  stay within `{index}` (search's `control.rs`, `per_index_routes`; feature/roll-search 5db7c2602
  and fae055c31), and a document an ingest pipeline moves (`_index`, `reroute`) is checked again
  as a write at the index it lands in (ea6be2f0f). ES|QL and SQL read as the caller too (0158b6d9c):
  `FROM`, wildcards, `LOOKUP JOIN` and SQL reach only the indices it may read.
- An approval about some indices names them (`cl_1/logs-a`), and remembering it allows those
  indices only.

### Secrets' encryption keys

A rule can grant some of a project's encryption keys: `{id: "prj_1", index: "payments,pay-*"}`,
names (letters, digits, `-`, `_`) and `prefix*` patterns. Unlike indices, names are literal: a
request names one key, so `_all`, a list or a `*` in a key's place reaches no key, and an approval
remembers only a request that named one. The builder writes it `prj_1/payments,pay-*` and lists
a project's keys to tick, as it lists a cluster's indices.

It covers the routes of one key (`/v1/secrets/projects/{project}/transit/{name}/…`: encrypt,
decrypt, sign, verify, HMAC, the public key), at their own actions, so a service that signs with
one key needs write on that key only, not on the project. Listing, creating, rotating and deleting
keys stay the project's (`roll_auth::scope::sub_resources` lists the products that have such
names; secrets' rows name `{name}` with `.index("name")`).

### Presets

The builder starts from a preset and the user narrows or widens it (`GET /v1/access/catalog` has
them; `project` means the workspace project the user picks).

| preset | rules |
| --- | --- |
| Read only | every product, `*`: `read` |
| Agent, asks before destructive (the consent screen's default) | every product, `*`: `read` `write`; `delete` `reveal` `deploy` ask |
| This project only | every product, the project: `read` `write`; `delete` `reveal` `deploy` ask |
| Dev environment only | every product, the project, environments `development`: `read` `write` `delete`; `reveal` ask |
| Previews only | every product, `*`, environments `preview`: `read` `write` `delete`; `reveal` ask |
| Full access | every product, `*`: every action (expiry still applies) |

## Evaluation

One function, `roll_access::evaluate`, in front of every route rolld serves. On every request:

1. **The token.** Look up the presented token's SHA-256 in the store. There is no cache, so a
   revoked token fails on its next request, on every node. The operator token and products' own
   credentials skip this; a request with neither passes or fails as it always has.
2. **The grant.** It must not be revoked or expired. The client's address must be in `ip_allow`
   when that's set (the peer address, or the edge's forwarded address when rolld is behind the
   edge, which hands the client's address over as the connection's own). `X-Forwarded-For` counts
   only from the peers in `ROLL_TRUSTED_PROXIES` (CIDRs or addresses: a load balancer in front of
   the edge), every line of it, and the client is its right-most entry that isn't one of them; an
   entry that doesn't parse stops the walk at the peer. With none listed the header counts for
   nothing, so a request that reaches the API listener directly can't name its own address. A
   wrong entry stops rolld. MCP tool calls carry the address the gate worked out, and the same
   address is what open registration's per-network limit counts.
3. **The route.** The matched route pattern and method find the catalogue row: an exact row (one
   naming the method beats `*`), else the longest prefix row. Routes that are a product's own
   router in disguise (tasks' `{*rest}`, storage's `nest_service`) are matched by the request's
   real path. The row gives the product, the action (from the body where it depends on it), and
   how to find the resource. No row: 403 `operator_only`.
4. **The rules.** Collect the rules whose product and resources match. The action's outcome is the
   most permissive among them (`allow` beats `ask`); no rule with the action means 403.
5. **Ask** pauses the request for a person ([below](#approvals)).
6. **Allow** sends the request on as the operator would make it, with the grant in its extensions
   (`roll_auth::Grant`: id, name, kind) so the audit log and products know who it was. Lists and
   creates are filtered or placed as above.

The decision is the same wherever the request comes from: the HTTP API, the CLI, or an MCP tool
(whose call is one request through the same router, with the same token).

A request forwarded to another node is evaluated again there, which is cheap (one indexed
read): roll code's repo owners get the request as it was, and the signed hops to storage's metadata
owner and the tasks cell's leader (`/internal/storage/control`, `/internal/tasks`) are decided as
the original request their header carries (`x-roll-storage-uri`, `x-roll-tasks-uri`). Those routes
take only signed requests, so nobody outside the cluster can put a path there. On the owner:

- the client's address is the one the first node saw (`x-roll-code-client`, signed), so IP
  allowlists hold on a fleet;
- a node that allowed the request for a grant says so in the hop's signature
  (`x-roll-code-decided: <grant id>`, `roll_cluster::Decided`). For that grant the owner doesn't
  ask a person a second time, and doesn't log the request again; it still denies whatever the
  grant's rules deny there, and a revoked token stops there too. A hop from a node that didn't
  decide it (a public listener in front of the tasks leader) is decided in full.

On an open daemon (loopback, no `ROLL_ADMIN_TOKEN`), requests without a token still pass, and a
request **with** a scoped token is still evaluated. That keeps a well-behaved client to its rules,
but it contains nothing: whatever holds the token can drop it and act as the operator. Scoped
tokens protect a daemon that requires `ROLL_ADMIN_TOKEN`; the dashboard says so on an open one.

## Grants outside a request

A product that acts later on a grant's behalf (secrets' syncs push with their creator's access)
looks the grant up when it acts, through `roll_auth::Grants` (rolld gives it roll-access): the
grant as it is then, `None` once it's revoked, expired or gone, and `Grant::allows` for what it may
do. Never cache it.

## Approvals

An `ask` outcome creates an approval and holds the request:

```ts
type Approval = {
  id: string;            // apr_…
  grant_id: string; grant_name: string;
  product: Product; action: Action; resource: string | null;
  method: string; route: string; path: string;
  summary: string;       // "DELETE /v1/projects/{id}: drop Postgres project acme-db"
  args: Json;            // the body, secrets redacted
  tool?: string;         // the MCP tool that made the call
  state: "pending" | "approved" | "denied" | "expired" | "used";
  created_at: number; decided_at: number | null; decided_by: string | null;
  expires_at: number;    // 10 minutes after created_at
};
```

- An approval is bound to the method, path and body, to what the request is about, and to the
  headers that change it (`X-Roll-Account`, `X-Roll-Project`, `X-Roll-User`, `X-Roll-Agent`).
- The request waits up to 55 seconds for a decision. Approved, it runs exactly as sent and its answer
  is the request's answer. Denied, it gets `403 {"code": "denied"}`.
- Still pending after the wait, it gets `403 {"code": "approval_pending", "approval": "apr_…",
  "url": …}`. Sending the same request again (with `X-Roll-Approval: apr_…`, or without: the gate
  finds the grant's approval for exactly that request) waits again, is refused with the person's
  reason if they denied it (for the approval's ten minutes, rather than asking them again), or runs it
  once approved. An approval runs one request: the one whose method, path and body hash it holds.
  It can't be reused.
- A token's waiting reads and writes (two or more, none with a body to review) can be approved
  together from the header's waiting list, each once; delete, deploy, reveal and admin are
  approved one by one.
- The person decides on the dashboard (a notification in the header, and the list under
  `/dashboard/access/approvals`), with `roll mcp approvals` / `roll mcp approve <id>`, or through the
  API. Only the operator decides. A grant never approves its own request, not even with `admin` on
  `access`.
- An approval not decided in 10 minutes expires.
- MCP tools hold the call open the same way, with keep-alive events, and return the pending
  approval as a result the agent can wait on (`access_wait_approval`).

### Ask for anything else

A token can be made so that what its rules don't allow asks a person instead of being refused
(`ask_otherwise`; the builder's and the consent screen's "Ask me for anything else", `roll token
create --ask-otherwise`), as Claude Code asks before anything it isn't allowed to do. Start an agent
read-only, and approve what it asks for once, for a while or always (below): the token grows with
what a person said yes to.

- It's still refused, never asked: the operator's routes (no row in the catalogue), a request from
  the wrong address, a revoked or idle token, and moving a resource between projects.
- `tools/list` shows such a token every tool of the products that are on.
- Only a token that asks may make one that does.

### Remembered approvals

Approving can also allow what's asked again, as Claude Code's and Cursor's "allow for this
session" or "always" do (`POST …/approve {remember: "1h" | "8h" | "1d" | "always"}`, the approvals
page's "Once ▾", `roll mcp approve --remember 1h`):

- It allows the request's action on its one resource (`{product, resources: [{id}], actions:
  {<action>: "allow"}}`); nothing broader. A request about the product as a whole or about
  something new ("new in a project") can only be approved once.
- For a while, the rule is the grant's `temporary`: it lapses by itself, and decisions read the
  grant's rules plus the live temporary ones. For good, it's added to the grant's `rules`, and the
  operator's token policy applies.
- A while never becomes for good: a token makes tokens covered by its own rules only, never by
  what it's allowed for now.
- Each rule for a while names the approval that allowed it. The operator takes it back before it
  lapses from the token's page or `roll token take-back <id> <approval>`
  (`DELETE /v1/access/tokens/{id}/temporary/{approval}`), and with it
  the approval when its request hasn't used it (the rule may have run that request instead).

### Approvals in chat

Approvals can also go to a Slack or Discord channel, through roll code's chat ops
(docs/code/chatops.md) and its install's bot, rather than a second integration:

- The operator picks an install, a channel and the people who may decide there
  (`PUT /v1/access/notify {"chat": {"install": "chi_…", "channel": "C…", "approvers": ["U…"]}}`,
  `roll mcp notify --install … --channel … --approver …`, or the approvals page). Each new approval is posted there as a card:
  who asks to do what, what would run (route, tool, arguments redacted), and the link. When it's
  decided, anywhere, the card is edited to say who decided what, and loses its buttons.
- A press arrives like every chat action (verified, recorded once by its id, handled by a worker;
  `code-control/src/chatops/access.rs`). roll-access decides it only when it came from the install
  approvals go to and from one of the approvers: not whoever else can see the channel (a Slack
  Connect guest, an install open to anyone), and not the install's member list, which the roll
  code owner manages rather than the operator. Anyone else is told so. `decided_by` is the
  person (`slack:U123`, `discord:123…`).
- Once a day, the live tokens that expire within a week are posted there too (`expiry_digest`,
  from the hourly maintenance loop; one node posts it, the day claimed in the store).
- Posting is best effort: an approval lives in roll (the dashboard, the API, `roll mcp approvals`);
  chat is one more place to decide it.
- Nothing about an approval's binding changes: it still runs exactly the request it was asked
  for, once, within ten minutes of the decision.

## Audit

Every request made with a grant, and every MCP tool call by anyone, writes one audit row:

```ts
type AuditEvent = {
  id: string; at: number;
  grant_id: string | null; grant_name: string | null; // null: the operator
  source: "api" | "mcp";
  tool: string | null;
  method: string; route: string;      // the route pattern, never the path
  product: Product | null; action: Action | null; resource: string | null;
  decision: "allow" | "deny" | "ask";
  approval: string | null;
  status: number;                      // the HTTP status the call ended with
  args: Json | null;                   // MCP arguments, redacted
  ip: string | null;
  duration_ms: number;
  client?: string;                     // what made it, as it says: an MCP client's name and version
                                       // (from initialize, carried in the session id), else its
                                       // User-Agent; one line, at most 120 characters
};
```

- The resource id is recorded (the user needs to know which database was dropped); paths, query
  strings and headers are not.
- Arguments are redacted before they're stored: any field named like a password, secret, token,
  key, credential or value has its value replaced with `"[redacted]"`, and strings are cut at 2 KiB.
- The log is kept for 90 days (`ROLL_ACCESS_AUDIT_DAYS`; older rows are deleted 5,000 at a time,
  so a large prune never holds the database's write lock) and read through `/v1/access/audit`,
  filtered by grant, product, decision and time.

Each node writes its audit rows through one writer, which takes what's queued (up to 512 rows)
and writes it in one statement (`Store::queue_audit`): a write per request, serialized, held a
token's requests to ~9k a second on one node; batched, ~110k (`gate_latency`, release, 32
callers), every row kept.

- A full queue (4,096 rows, which drain well inside sinks' 30-second settling time, so no row lands
  behind a sink's cursor) has the request write its own row: nothing is dropped.
- A batch that fails is written row by row, so one bad row loses only itself; a NUL (a path's
  `%00`) is dropped from text, since Postgres refuses it.
- At shutdown, rolld waits (up to 5 seconds) for the queue to be written. As before, these writes
  aren't synced: a crash can lose the last moment's rows.

### Audit sinks

The operator can stream the log to a SIEM (Datadog, Splunk, a Lambda): the audit page's "Stream to
a SIEM", `roll mcp sinks add https://…`, or `POST /v1/access/audit/sinks {url, from_start?}`
(`roll-access/src/sinks.rs`).

- A sink is a public HTTPS URL (roll-net's guard, no redirects, 10 s, the answer read to 64 KiB).
  It gets new rows in order, in batches of up to 200: `{sink, source: "access", events:
  AuditEvent[]}`, signed as every roll webhook is (roll code's), so one receiver takes all of
  them: `X-Roll-Signature-256: sha256=<hex HMAC-SHA256 of "{timestamp}.{body}">`,
  `X-Roll-Timestamp: <unix seconds>`, `X-Roll-Delivery` (the same when a batch is sent again, so a
  receiver drops what it has) and `X-Roll-Sink: <id>`.
- The signing secret (`whsec_…`) is shown once and kept sealed with `ROLL_SECRET_KEY`, bound to
  its sink; without the key, streaming is off. `ROLL_ACCESS_SINK_ALLOW_PRIVATE` allows plain http
  on a private address (local development only).
- Each sink keeps a cursor (the last row's time and id). Every node sweeps every 10 seconds, sending batches until a
  sink has no full one waiting (up to 5,000 rows a sink a sweep); a claim in the store keeps a
  batch to one node, and only rows 30 seconds old go (a row is written
  just after its request, so a cursor never passes one still being written). A failed batch is
  sent again 5 minutes later, or at once with "Retry now" (`POST …/sinks/{id}/retry`). Rows the
  log prunes before a sink catches up are gone. At most 10 sinks.

## Managing grants

- The operator manages grants. A grant may too when it has `admin` on product `access`, and then
  only grants it covers entirely, before and after a change: rules within its own, an expiry no
  later than its own, (when it has an IP allowlist) addresses within its own, and (when it has a
  rate limit) a limit no higher than its own. It can never make a token more powerful than it is,
  one that outlives it, one usable from elsewhere or one that may make more requests, and never
  changes its own addresses.
- A grant reviews and comments in roll code only as itself (`token:<name>`): the gate drops
  `X-Roll-User` and sets `X-Roll-Agent`.
- The operator's policy (`/v1/access/policy`, `roll token policy`) caps every grant's lifetime
  (`max_days`) and can require that every grant expires; a grant made or changed after must fit,
  OAuth connections included (GitHub's organisation token policies, Cloudflare's TTLs).
  `max_idle_days` stops a grant nobody has used for that long (since its last use, or since it was
  made): an unused credential is risk with nothing to show for it. The evaluator reads the policy at
  most every 10 seconds, so a change reaches every node within that.
- The secret is shown once, at creation. The store keeps its SHA-256.
- A product that makes tokens of its own for its caller (roll secrets' project tokens) makes them
  with `Access::create_grant` and the caller's request: the function `POST /v1/access/tokens` is,
  with the same owner, coverage, policy and caps, never a second way in. Its page lists them with
  `GET /v1/access/tokens?product=&resource=`.
- `last_used_at` and `last_used_ip` are written at most once a minute per grant.

### Tighten to what it used

A token's page (or `roll token tighten <id> [--apply]`) proposes its rules narrowed to what its
requests used in the last 30 days, allowed ones and ones that ran on an approval
(`GET /v1/access/tokens/{id}/tighten?days=`, `roll_access::tighten`), as AWS's access analyzer
writes a policy from activity:

- Each proposed rule is within the rule it came from: the same product (one rule per product a `*`
  rule was used for), environments, paths and effects (`ask` stays `ask`), only the actions used,
  and the resources used, by id, when the rule named every resource or ids (at most 20).
- A rule placed by project keeps its project: a resource can leave the project, and its id would
  still match. An action used without a resource (a list, the product as a whole) keeps the rule's
  resources. What a person allowed for a while is never made permanent.
- The server checks the proposal is covered by the token's rules and passes the rules' own checks,
  else proposes them unchanged; a token that made no allowed requests is proposed unchanged (revoke
  it instead).
- Applying it (`POST …/tighten {from, days?}`, the operator's) makes the proposal again and swaps
  the rules only if they're still `from`, the rules it was shown from: a change in between is 409.
- What it went by: one audit row per request, plus one per second check (a copy's or a link's
  reveal), so an action a request needed twice isn't dropped. A request that ran on a rule for a
  while counts for a rule with the same product and action, which can keep an action that rule
  didn't need; it never widens one.

### Checkup

The access page opens with what needs attention (`GET /v1/access/checkup`, `roll_access::checkup`,
`roll token checkup`, which prints each fix's command),
as Google's security checkup and GitHub's security overview do: each live token with a finding,
most serious first, and its fix.

| finding | when | fix |
| --- | --- | --- |
| `everything` | a rule allows delete, reveal or admin on every product's resources, without asking | tighten |
| `never_expires` | no expiry | give it one |
| `unused` | no request in 30 days (or never, 30 days after it was made) | revoke |
| `narrower` | its requests in the last 30 days needed less than its rules allow (its tighten proposal is narrower; checked for up to 200 recently used tokens) | tighten |
| `roams` | used from five or more networks, with no address limit | limit its addresses |

### New networks

A token used from a network it hasn't been used from before is told where approvals go (GitHub
and Google mail a sign-in from somewhere new): a leaked token is used from somewhere new.

- A network is an address's /24 (IPv4) or /48 (IPv6), so one office or one pool of runners is one;
  an IPv4 client on a dual-stack listener (`::ffff:a.b.c.d`) is its IPv4 network.
- Its first network is where it starts, and isn't news. Once a token has been used from five
  (`ROAMING`: CI runners, phones), it roams; the message about the fifth says that's the last.
- Each node writes a token's use at most once a minute per network, so a new network is seen on its
  first request, and the row that's inserted decides what's new (two nodes tell one new network
  once). The token's page lists its networks with when each was first and last seen
  (`GET /v1/access/tokens/{id}/networks`); one unused for the log's days is forgotten.
- Names in these messages, and in the daily expiry list, are plain text: markup a person or an
  OAuth client put in a name (Slack's `<!channel>`, Discord's `[label](url)`) is dropped, and Slack
  text is escaped.

## Leaked tokens

Other products' secrets made the same way are found and revoked the same way: apps' deploy hooks
(`rlk_`, the secret in a hook's URL) are in `SECRET_PREFIXES`, and a product registers how to
revoke its own with `Access::on_leaked` (rolld wires apps' when apps is in). The audit log has each
revocation.


A token pasted into a file is a token anyone who reads that file holds. roll revokes one as soon as
it's found, as GitHub does for its own tokens:

- **In roll code.** Everything that lands, in any repository, is read after it lands. The scanner
  reads the store's durable land log, never inside the lander, with a cursor of its own and one
  node at a time (`code-control/src/leaks.rs`). It reads every changeset a landing added (up to
  50), and the changed files up to 1 MiB each, at most 500 files and 64 MiB a landing; a file it
  can't read is skipped, never retried forever. A file that mentions a token prefix goes to
  roll-access, which matches checksums and revokes each live token. A token found in a private
  repository says only that and the repository's id (whoever owns the token may not read it).
- **From GitHub.** `POST /v1/access/leaks/github` takes reports from GitHub's secret scanning
  partner program: tokens found in public repositories, gists and issues. Each report is signed
  with one of GitHub's ECDSA P-256 keys (`Github-Public-Key-Identifier` and
  `Github-Public-Key-Signature`, checked against
  `api.github.com/meta/public_keys/secret_scanning`: cached for an hour, fetched again at most once
  a minute and one fetch at a time, and a failed fetch leaves the keys there were). roll revokes the live tokens and answers each with its
  `token_hash` and `true_positive` or `false_positive`. An unsigned or wrongly signed report
  changes nothing.
- A revoked token answers 401 on its next request. The grant keeps `revoked_why` ("found in
  acme/site .env at 1a2b…", shown in the dashboard), and the audit log gets a row (`source:
  "leak"`, `decision: "revoke"`).

## Rate limits

A grant may have `rate_per_minute` (the builder's "Requests a minute", `roll token create --rate`,
the consent screen), like Stripe's and GitHub's per-key limits. It protects roll from a runaway
agent loop; it isn't billing.

- A GCRA per grant (`roll-access/src/rate.rs`): a full minute's worth may come at once, then one
  request every `60 s / rate`. Counted in memory on each node, with no shared write per request:
  each of N serving nodes enforces its share, the rate over N rounded up (`Access::rate_share`, N
  from the cluster's live nodes), so the fleet allows about the rate. A client whose requests all
  reach one node gets that node's share. The `RateLimit-*` headers give the share this node enforces.
- Every MCP tool call counts, as the route it calls; the `/v1/mcp` envelope doesn't, so a batch of
  calls (at most 100 messages) counts each call. A hop to an owner counts where it was decided.
- Allowed answers carry `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset` (seconds).
  Over the limit the answer is `429` `{code: "rate_limited", retry_after, limit}` with
  `Retry-After`, before anything is looked up or written: no audit row, no `last_used_at`, so a
  flood stays cheap. Over MCP the `/v1/mcp` request itself answers that `429` (the MCP SDKs raise it
  with its body, so the agent sees when to retry).

## Storage

On a single node the data is SQLite at `<home>/access.db`; with the system database
(`ROLL_SYSTEM_DATABASE`) it's that database's `access` schema, shared by every node without
leases (each read-then-write is one `code_sql` write).

```sql
grants(id PRIMARY KEY, name, kind, rules JSON, expires_at, ip_allow JSON, created_by, created_at,
       last_used_at, last_used_ip, revoked_at, client_id)
secrets(hash PRIMARY KEY, grant_id, kind /* token | access | refresh */, expires_at, created_at)
approvals(id PRIMARY KEY, grant_id, …, request_hash, state, created_at, decided_at, decided_by, expires_at)
audit(id PRIMARY KEY, at, grant_id, …)
oauth_clients(id PRIMARY KEY, name, redirect_uris JSON, uri, created_at, last_used_at)
oauth_codes(hash PRIMARY KEY, client_id, grant JSON, redirect_uri, challenge, resource, expires_at)
```

## HTTP API

Under `/v1/access`. Bodies are JSON; errors are `{"error": string, "code"?: string}`.

| route | who | what |
| --- | --- | --- |
| `GET /v1/access` | any grant | the caller: the operator, or its own grant (rules, expiry) |
| `GET /v1/access/catalog` | any grant | products, actions, presets and every classified route |
| `POST /v1/access/check` | any grant | `{method, path, body?, token?}` → `{decision, product, action, resource, route}`: what the evaluator would decide, without running it (a grant checks itself; the operator names `token`) |
| `GET /v1/access/tokens` | access read | grants, newest first (no secrets), each with `activity`: its last day's requests `{allowed, denied, asked}`; `?product=` (and `&resource=`) only those with a rule on that product by name (naming that `{id}` or `{project}`), as a product's page lists its own tokens |
| `POST /v1/access/tokens` | access admin | `{name, rules, expires_at?, ip_allow?, rate_per_minute?}` → 201 `{grant, token}`; `token` is shown this once |
| `GET /v1/access/tokens/{id}` | access read | a grant |
| `PATCH /v1/access/tokens/{id}` | access admin | `{name?, rules?, expires_at?, ip_allow?, rate_per_minute?}`: narrow or widen (within the caller's own); `rate_per_minute: null` takes the limit off |
| `DELETE /v1/access/tokens/{id}` | access admin | revoke, at once |
| `DELETE /v1/access/tokens/{id}/temporary/{approval}` | operator | take back what an approval allowed it for a while, and the approval if its request hasn't used it |
| `POST /v1/access/tokens/{id}/tighten` | operator | `{from, days?}`: apply the proposal, made again now, if its rules are still `from` (else 409) → the grant |
| `GET /v1/access/checkup` | access read | the live tokens that need attention: `[{grant, findings: [{kind, why, fix}]}]` ("Checkup") |
| `GET /v1/access/tokens/{id}/networks` | access read | the networks it's been used from: `[{network, first_at, last_at}]`, newest first ("New networks") |
| `GET /v1/access/tokens/{id}/tools` | access read | what an agent connected with it is offered over MCP: `{tools: [{name, title, product, read_only, destructive}], prompts, context_tokens}` (served by roll-mcp, which reads the token as the caller first); the token page shows it |
| `GET /v1/access/tokens/{id}/tighten` | access read | `?days=` (1-90, 30): `{since, days, used: [{product, action, resource, count, last_at}], rules, narrower}`: its rules narrowed to what it used ("Tighten to what it used") |
| `POST /v1/access/tokens/{id}/rotate` | access admin | a new secret with the same rules (the old one stops at once) → `{grant, token}`; not for OAuth connections |
| `GET /v1/access/approvals` | access read | newest first; `?state=pending`, `?grant=<id>` (one token's), `?limit=` (at most 500) |
| `GET /v1/access/approvals/{id}` | the grant that asked, or access read | an approval; `?wait=55` waits for a decision |
| `POST /v1/access/approvals/{id}/approve` | operator | `{remember?: "1h" \| "8h" \| "1d" \| "always"}`: also allow the same action on the same resource ("Remembered approvals") |
| `POST /v1/access/approvals/{id}/deny` | operator | `{reason?}` |
| `GET /v1/access/audit` | access read | `?grant=&product=&action=&decision=&source=&resource=&before=&limit=`; `resource` is who touched it: that resource or anything under it (`cl_1` finds `cl_1/logs-a`); `&format=jsonl` answers one event per line and `&format=csv` a row each (UTC times, every cell quoted; a formula character is defused with `'` at a cell's start and after a `;`, tab or `|` inside it, which some spreadsheets split on), as a download (the audit page and `roll mcp audit --format csv` or `jsonl`) |
| `POST /v1/access/leaks/github` | public (GitHub's signature) | GitHub secret scanning reports: `[{token, type, url, source}]` → `[{token_hash, token_type, label}]`; live tokens are revoked |
| `GET /v1/access/audit/sinks` | operator | where the log streams: `Sink[]` (`{id, url, status, error, cursor_at, delivered_at, created_at, created_by}`) |
| `POST /v1/access/audit/sinks` | operator | `{url, from_start?}` → 201 `{sink, secret}`; the secret is shown this once |
| `POST /v1/access/audit/sinks/{id}/retry` | operator | a failed sink's batch at the next sweep → 204 |
| `DELETE /v1/access/audit/sinks/{id}` | operator | stop streaming → 204 |
| `GET /v1/access/policy` | operator | `{max_days, require_expiry, max_idle_days}`: roll's rules for every grant |
| `PUT /v1/access/policy` | operator | change them; a grant made, rotated, or whose rules, lifetime or addresses change, must fit (consent too) |
| `GET /v1/access/notify` | operator | where approvals are posted: `{chat: {install, channel, approvers} \| null}` |
| `PUT /v1/access/notify` | operator | `{chat: {install, channel, approvers} \| null}` |
| `GET /v1/access/oauth/clients` | access read | MCP clients with a live connection |
| `GET /v1/access/oauth/clients/{id}` | access read | a registered client (the consent screen shows it); only the operator's lookups fetch a metadata document |
| `POST /v1/access/oauth/authorize` | operator | the consent screen's answer: `{client_id, redirect_uri, code_challenge, state, resource, name, rules, expires_at?}` → `{redirect}` |

OAuth's public endpoints (discovery, registration, token, revocation) are in [mcp.md](mcp.md#authorization).

## Classification

Every route rolld serves is in the catalogue, or deliberately left to its own credentials. At
startup rolld logs each route with its access and its classification
(`RUST_LOG=rolld::auth=debug`: `route=/v1/projects scoped=postgres access=operator`), and
`crates/rolld/tests/auth.rs` fails on any route that no grant could be classified for, outside
the list below. It also fails on a method decided only by a prefix row (`/v1/previews/{id}/`, by
method: no row of the route's own names that method; rolld logs each route's methods) that isn't
in `crates/roll-access/src/prefix-reviewed.txt` as `METHOD route`: such a route takes the prefix's
action whatever it answers (a preview's variables, with their values, were a read until they got
a row of their own), so each new one is either given its own row or looked at and listed. The
operator's routes (public routes, which anyone may call without a token, aren't a grant's to
classify, so the check skips them):

| routes | why |
| --- | --- |
| `/t/…`, `/_roll/tenants` | hosted tasks tenants: their own API keys and the tasks token |
| `/kafka/v3` | Confluent REST: stream API keys |
| `/git` | git smart HTTP: roll code tokens |
| `/internal/…` | signed hops between nodes |
| `/v1/code/chat`, `/v1/code/github`, `/v1/code/linear` | platforms' signed webhooks and install flows |
| `/v1/code/agents/connect`, `/v1/code/agents/login` | machines' relay link and device login |
| `/v1/code/agents/shares/{id}`, `/v1/code/names` | public pages |
| `/v1/code/review-apps` | review apps' own tokens |
| `/v1/accounts` | people's accounts (docs/accounts.md): sign-in, sessions and invites are a person's, never a token's |
| `/v1/code/mcp` | roll code's MCP server calls roll code without the gate; grants use `/v1/mcp` |
| `/v1/access/approvals/{id}/approve` and `/deny`, `/v1/access/oauth/authorize`, `/v1/access/notify`, `/v1/access/policy`, `/v1/access/audit/sinks` | the operator decides |

Rows match a route by its shape, not its parameters' names (`…/secrets/{key}/value` finds the row
written `…/secrets/{name}/value`), so renaming a parameter can never change an action. To check a
branch's routes before it merges, list them as `METHOD<TAB>PATTERN` lines and run
`cargo run -p roll-access --example classify < routes.txt`: each line comes back with its product,
action and row, `NONE` for a route no grant could call, and `MISSING PARAMS` when a row's resource
or environment parameter isn't in the route.

The rest of the work list is in [mcp.md](mcp.md#next).
