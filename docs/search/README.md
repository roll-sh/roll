# roll search

Elasticsearch-compatible search clusters that cost nothing while idle, wake in milliseconds,
branch instantly and keep their data on object storage. Clients speak the Elasticsearch REST API
(or OpenSearch's): the official clients, searchkick, Laravel Scout and Spring Data work by
changing the URL. Semantic and hybrid search work as on Elasticsearch 9: `semantic_text` fields
embed with no key through roll's own multilingual-e5-small (the preconfigured
`.multilingual-e5-small-elasticsearch` endpoint, run on the node's CPUs as Elasticsearch's ML
nodes run it), or through the model provider you configure (`_inference`: OpenAI or any
compatible server, Cohere, Anthropic, …; the key sealed in the cluster), and `rrf`/`linear`
retrievers mix them with lexical search.

Guides for people building on roll search, one per workload, start with the first query in a
minute: [guides/](guides/README.md) (site search, e-commerce, logs, RAG, multi-tenant SaaS). This
README is the operator's and internals' view.

Design: [architecture.md](architecture.md). Cross-crate interfaces: [contracts.md](contracts.md).
On AWS search runs on the roll cloud fleet with every other product (docs/cloud/README.md).

## Run it

`rolld` serves search next to the other roll services:

```sh
cargo run -p rolld                      # REST on 127.0.0.1:59200, roll API on 127.0.0.1:7070
pnpm dev                                # dashboard: /dashboard/search
```

| flag (env)                                         | default           | |
| -------------------------------------------------- | ----------------- | - |
| `--search-http` (`ROLL_SEARCH_HTTP`)               | `127.0.0.1:59200` | Elasticsearch REST listener |
| `--search-tls-cert`, `--search-tls-key`            |                   | serve HTTPS (PEM) |
| `--search-domain` (`ROLL_SEARCH_DOMAIN`)           |                   | clusters at `<name>.<domain>` (SNI / `Host` routing) |
| `--search-idle-unload` (`ROLL_SEARCH_IDLE_UNLOAD`) | `300`             | seconds without requests before a cluster sleeps (default for new clusters) |
| `--search-cache-bytes`                             | 10 GiB            | local disk cache for segments |
| `--search-sync-objects`                            | off               | acknowledge writes once the WAL is in object storage (ephemeral disks) |
| `--search-proxy-protocol`                          | off               | PROXY protocol headers (behind an NLB or HAProxy) |
| `--search-client-ip-header` (`ROLL_SEARCH_CLIENT_IP_HEADER`), `--search-trusted-proxies` (`ROLL_SEARCH_TRUSTED_PROXIES`) | | the header a trusted proxy writes the client's address in (`CF-Connecting-IP`), believed only from those CIDRs; for IP allowlists (architecture.md, "Client addresses and IP allowlists") |
| `--search-history` (`ROLL_SEARCH_HISTORY`)         | `86400`           | point-in-time branching window, seconds (0: off) |
| `--search-gc-interval`                             | `3600`            | garbage collection of unreferenced objects (0: off; one node per bucket) |
| `--search-deleted-retention-days` (`ROLL_SEARCH_DELETED_RETENTION_DAYS`) | `7` | days a deleted cluster can be restored, its storage billed meanwhile (0: deleted at once) |
| `--search-test-features`                           | off               | publish Elasticsearch's test features (YAML suites through `--cluster-hook`; test deployments only) |
| `--search-max-time` (`ROLL_SEARCH_MAX_TIME`) | `60`, none on an open daemon | seconds one request that searches may run, all its shards and phases together (node setting `roll.search.max_time`; 0: no cap; compatibility.md "Operations") |
| `--search-max-clusters-per-person` (`ROLL_SEARCH_MAX_CLUSTERS_PER_PERSON`) | `50` | clusters a signed-in person may own, branches included (0: no cap; the operator and admins aren't capped) |
| `--search-max-always-on-per-person` (`ROLL_SEARCH_MAX_ALWAYS_ON_PER_PERSON`) | `2` | of those, always-on ones (`idle_timeout_s` 0) (0: no cap) |
| `--search-audit-retention-days` (`ROLL_SEARCH_AUDIT_RETENTION_DAYS`) | `7` | how long each cluster's security audit log keeps its entries |
| `--search-audit-max-mb` (`ROLL_SEARCH_AUDIT_MAX_MB`) | `32` | audit entries kept per cluster, compressed MiB; past it the oldest go |
| `--search-embed-model` (`ROLL_SEARCH_EMBED_MODEL`) | `e5`              | `off`: the E5 endpoint answers as Elasticsearch without ML nodes (429), and no weights are fetched |
| `--search-embed-url` (`ROLL_SEARCH_EMBED_URL`)     | pinned Hugging Face revision | where the model's two files are fetched from, once, into `<home>/search/models` (`<url>/model.safetensors`, `<url>/tokenizer.json`, checked against the pinned SHA-256 digests) |
| `--search-embed-dir` (`ROLL_SEARCH_EMBED_DIR`)     |                   | read the model's files from this directory instead (air-gapped nodes; same digests) |
| `--search-embed-workers`, `--search-embed-cpus` (`ROLL_SEARCH_EMBED_WORKERS`, `ROLL_SEARCH_EMBED_CPUS`) | a third of the CPUs, not pinned | the embedding pool's threads, and the CPUs they're pinned to (`0-5,18-23`) |
| `--search-embed-idle` (`ROLL_SEARCH_EMBED_IDLE`)   | `300`             | seconds without inputs before the model leaves memory |
| `--search-embed-queue`, `--search-embed-queue-mb` (`ROLL_SEARCH_EMBED_QUEUE`, `ROLL_SEARCH_EMBED_QUEUE_MB`) | `4096`, `64` | inputs, and MiB of them, one cluster may have queued; past either a call gets 429 `es_rejected_execution_exception` |
| `--search-embed-node-queue-mb` (`ROLL_SEARCH_EMBED_NODE_QUEUE_MB`) | `512` | MiB of inputs every cluster's queues hold together; past it a call gets 429 |
| `--search-embed-max-inputs` (`ROLL_SEARCH_EMBED_MAX_INPUTS`) | `2048` | inputs per inference call (400 past it); each input is cut to the start its 512 tokens come from, so its length isn't capped |
| `--no-search`, `--search-only`                     |                   | shorthands for `ROLL_PRODUCTS` without search, or with only search |
| `--search-public-url` (`ROLL_SEARCH_PUBLIC_URL`)   |                   | the front clients use (a load balancer): cluster URLs name it |

Search is one of rolld's products (`ROLL_PRODUCTS`, `crates/rolld/src/products.rs`): `/v1/search`
sits behind rolld's operator token (`ROLL_ADMIN_TOKEN`) like every product's API. On a cluster
(roll cloud: `ROLL_CLUSTER_SECRET`, a cluster backend, `ROLL_SYSTEM_DATABASE`, S3) every search
cluster is served by the holder of its lease `search:cluster:<name>`, its catalog is in the system
database and any node's REST port forwards a request to the owner (architecture.md, "On the
fleet").

Storage is rolld's: `--s3-bucket` (with `--s3-prefix`, `--s3-endpoint`), or `<home>/objects`.
Local disk under `<home>/search/` holds `search.db` (the control plane's SQLite) and the cache.

The built-in embedding model (`crates/search-embed`, `crates/search-server/src/embed.rs`;
compatibility.md, "Inference"): `intfloat/multilingual-e5-small` (MIT), in Rust on the CPU (Hugging
Face's `tokenizers`, and `gemm` on one thread per worker; no Python, no GPU). A node fetches its two
files (`model.safetensors`, 471 MB, and `tokenizer.json`, 17 MB) once, on the first input any
cluster sends it, into `<home>/search/models/multilingual-e5-small/<revision>/`, from the pinned
revision or `--search-embed-url` (a mirror), and checks both against the SHA-256 digests pinned in
`search_embed::FILES` before loading them. Tenants never choose where weights come from. On an
air-gapped node copy the two files of that revision into a directory and pass `--search-embed-dir`;
the same digests are checked. `--search-embed-model off` turns it off. The model loads on first use
(about a second from the cache) and leaves memory after `--search-embed-idle` seconds without
inputs, so a node whose clusters don't embed holds nothing. Its memory (about 470 MB while loaded,
plus the batches running) counts against `--search-memory-mb`: clusters unload to make room for it.

## Use it

```sh
roll search clusters create app            # prints the URL, user and password (shown once)
curl -u app:<password> http://127.0.0.1:59200/
roll search request app PUT /products '{"mappings":{"properties":{"title":{"type":"text"}}}}'
roll search request app POST /products/_bulk @products.ndjson
roll search request app GET '/products/_search?q=title:shoes'
roll search clusters branch app app-preview              # instant copy, its own password
roll search clusters branch app app-yesterday --at 1d    # as of a day ago
roll search clusters indices app
roll search clusters usage app                           # cost vs Elastic Cloud / Amazon OpenSearch
roll search clusters password app | unload app | delete app | url app | show app | list
roll search clusters list --deleted                      # deleted in the last 7 days, with purge dates
roll search clusters restore srch_… [--name app-2]       # back as it was (under another name if app is taken)
roll search clusters update app --allow 203.0.113.0/24=office --allow 2001:db8::/32   # only these networks reach its endpoint
roll search clusters update app --allow-any                                          # anyone again
roll search index create app books '{"mappings":{"properties":{"title":{"type":"text"}}}}'
roll search docs put app books 1 '{"title":"dune"}' --refresh
roll search index bulk app books @books.ndjson --refresh
roll search index search app books --q title:dune
roll search docs get app books 1 | delete app books 1
roll search keys create app worker --expires-in 30d   # a data-only API key, shown once (--access admin for all)
roll search keys list app | revoke app <id>
roll search index show app books | mapping … | delete-by-query … | delete app books
```

Semantic search with roll's own model, no key (Elasticsearch's preconfigured E5 endpoint):

```sh
roll search request app PUT /articles '{"mappings":{"properties":{"body":{"type":"semantic_text","inference_id":".multilingual-e5-small-elasticsearch"}}}}'
roll search request app POST '/articles/_doc?refresh=true' '{"body":"Passwords can be reset from the sign-in page."}'
roll search request app POST /articles/_search '{"query":{"semantic":{"field":"body","query":"elfelejtettem a jelszavam"}}}'
```

Or through a provider (or the dashboard's Inference tab):

```sh
OPENAI_API_KEY=… roll search inference create app emb --model text-embedding-3-small   # key from the environment, sealed in the cluster
roll search request app PUT /articles '{"mappings":{"properties":{"body":{"type":"semantic_text","inference_id":"emb"}}}}'
roll search request app POST '/articles/_doc?refresh=true' '{"body":"Passwords can be reset from the sign-in page."}'
roll search request app POST /articles/_search '{"query":{"semantic":{"field":"body","query":"forgot my password"}}}'
roll search inference list app | test app emb "some text" | delete app emb
```

Answers generated from search hits (chat with citations, RAG) are designed in [rag.md](rag.md).

A cluster's name is its identity: the Basic auth user, the host label under `--search-domain`,
`cluster_name` in `GET /`. Names are DNS labels (lowercase letters, digits, `-`) and can't
change, except by restoring a deleted cluster under another name when its own was taken. Clients authenticate with Basic auth (`<name>:<password>`, or any user at the cluster's
own host), `Authorization: ApiKey base64(<name>:<password>)` or
`Authorization: Bearer <name>_<password>`.

## roll API

`/v1/search` on the rolld API port (`crates/search-control/src/api.rs`). Errors are
`{"error": "..."}` with 400, 404, 409, 413 (a console request over the cluster's body limit) or
500.

**Who reaches it** (docs/accounts.md, "Phase 2"). rolld's gate lets in the operator's token,
admins and every signed-in person (`Access::User`, `crates/rolld/src/auth.rs`), and search scopes
them (`api::Caller`):

- A cluster has an `owner`: the signed-in person (`usr_…`) who made it. The operator's and
  admins' clusters, and every cluster made before owners, have none (`null`). A branch keeps its
  parent's owner, whoever cuts it.
- The operator and admins see and manage every cluster.
- Anyone else lists only their own clusters, creates and branches clusters that are theirs, and
  gets 404 (`no search cluster <id>`, as for one that doesn't exist) from every
  `/v1/search/clusters/{id}/…` route about another's, and from a create whose `parent` is
  another's. The check reads the owner from the catalog after resolving the cluster, before a
  fleet forwards the call to the cluster's owner node and again there (behind its own gate).
- A person may own at most `--search-max-clusters-per-person` clusters (default 50) and, of
  those, `--search-max-always-on-per-person` always-on ones (`idle_timeout_s` 0; default 2); 0 is
  no cap. Branches count toward their owner (the parent's), whoever cuts them; clusters with no
  owner count toward nobody. Past a cap a person's create or branch, or a `PATCH` that makes one
  of their clusters always-on, answers 409 naming the cap, their count and the flag that raises
  it (for always-on: a cluster that sleeps when idle doesn't count). The operator and admins
  aren't capped, also when they cut a branch of a person's cluster. The count is
  `list_owned`'s, made in the insert's or update's own catalog transaction (SQLite's single
  writer; on a fleet Postgres `SERIALIZABLE`, which re-runs one of two racing creates), so two
  creates at the cap can't both pass. Lowering a cap removes nothing: a person over it keeps their
  clusters and can change an always-on one's other settings, but adds no more.
- `GET /v1/search` gives a person versions, endpoint, defaults, history window, pricing and their
  `caps` (`{clusters, always_on}`), but not where the node keeps its data (`storage`) or its
  node-wide figures (`loaded`, `cache`).
- A person's own token (docs/permissions.md, "Users' own tokens") acts as them: it reaches search
  (`search` is in `USER_REACH`) and sees only their clusters, narrowed again by its rules.
- Terraform (below) calls these routes as `roll search` does, with the same tokens: a person's
  configuration manages only their clusters and is held to their caps (a create past one fails
  with the cap's message).
- A person's workspace project stands only for their own clusters (`Workspace::trusts`: rolld's
  `Owners` reads the owner), so previews, roll secrets' links and roll apps' links take only
  those.
- A snapshot repository's credentials may be roll secrets references (`roll://ENV/KEY`,
  architecture.md "Snapshots"), but only through roll's API
  (`PUT /v1/search/clusters/{id}/repositories/{name}`, the cluster's `admin` for roll-access), by a
  caller roll secrets lets reveal each one (`Caller::allows`), who is kept as their maker. The
  cluster's own endpoint refuses them (400): its password or an API key with `manage` is no grant to
  read a project's secrets. Each use re-checks that the maker still may reveal them; only a cluster
  on a project's canvas that stands for it resolves one, and a branch resolves as an untrusted copy
  of the cluster it came from (no protected environment, no sensitive key).
- Nothing in the API is node-wide besides `GET /v1/search`'s figures: node settings and the
  default repository are rolld's flags (`--search-*`), not routes. A cluster's data plane is
  unchanged: the REST port with the cluster's own credentials, which give what any tenant gets.

`crates/search-control/tests/control.rs` `people_see_only_their_own_clusters` and
`people_are_held_to_their_cluster_caps` cover it.

**Branches and what they inherit.** A branch starts from a copy of its parent's state, its
recurring jobs and outside storage included. So that it never acts as its parent, a new
branch's first load (before any job can run) marks what it inherited (`search_es::branch`).
The catalog records a pending mark when the branch is made (`inherit_pending`), the control
plane passes the parent's id as `ClusterConfig::branch_of` only while it is pending, and a load
that marked clears it. Branches made before this existed have no pending mark: their jobs stay
their own. During a rolling deploy, a branch made by a new node and first loaded by an old one
runs as before, and is marked on its next load by a new node: jobs its owner added in between
are then paused too (put them again to resume them).

- Repositories in a bucket the tenant named (`s3`, `gcs`, `azure`) are registered read-only, as
  Elastic Cloud registers another deployment's repository. The branch lists and restores the
  parent's snapshots there; creating, cloning and deleting snapshots, `_cleanup` and SLM
  retention are refused with Elasticsearch's read-only repository errors. Registering a
  repository at an inherited place again keeps it read-only unless the request says
  `readonly: false` (the owner's decision, with the credentials sent again). A place is the
  bucket (an `azure` container, its default when unnamed) and the `base_path` without outer
  slashes, whatever the type or endpoint, so `prod/` is `prod`. The check guards against
  accidents, not against the owner: an `s3` `endpoint` naming a bucket's own host with
  `path_style_access` places keys in that bucket without matching. A repository anywhere else
  behaves as on any cluster. roll's default repository (`found-snapshots`) and `fs` repositories keep their
  contents in the cluster's own storage, which a branch has its own copy of: they stay writable,
  and the branch's snapshots and deletions there never reach the parent's.
- SLM policies stay visible but don't run, and their retention deletes nothing:
  `last_failure` says the policy was inherited, `next_execution` is left out and `_execute`
  answers 409. Putting the policy again makes it the branch's own. Until then an ILM
  `wait_for_snapshot` on one waits; an index that reaches the step on the branch has a
  `step_info` that says why (one already waiting when the branch was made keeps the parent's).
- Watches are inactive (`status.state.active: false`, as `_deactivate` leaves them), so a
  webhook or email isn't sent twice. Activating one is the branch owner's decision.
- Connectors stay paused: a sync reads its source with the parent's stored credentials, so no
  job of an inherited connector becomes due or starts on the branch (one already waiting stays
  `pending`, and its schedule makes none). Putting the connector again, or writing its
  configuration, makes it the branch's own, and its syncs run from then on, as an SLM policy
  put again does. The parent's credentials don't come along: the branch gives them again in
  that write (or later), since sealed values are bound to the cluster they were written on.
- ILM, ISM, data stream lifecycles, transforms and rollups work inside the branch's own indices
  and keep running. A step of theirs that writes to an inherited repository is refused like
  any write there, and ILM's `cleanup-snapshot` leaves a snapshot in one alone (it is the
  parent's).

`a_branch_runs_none_of_its_parents_jobs` in the same file covers it.

**IP allowlists** (Elastic Cloud's traffic filters, AWS OpenSearch Service's IP policies). A
cluster's `ip_allowlist` lists the networks that may reach its endpoint: at most 64 entries,
each a CIDR or address, IPv4 or IPv6, given as a string or `{cidr, description?}` (as roll
apps' `allow_ips`), kept canonical (`10.1.2.3/8` is `10.0.0.0/8`). `[]`, the default, admits
anyone. The cluster's owner sets it like any setting: `PATCH`, `roll search clusters update
--allow`/`--allow-any`, or the dashboard's Settings. It is the `PATCH` route's `admin` action
for roll-access. Every request to the endpoint is checked first, before its credentials and
before a sleeping cluster wakes. A request from elsewhere gets 403
`{"ok":false,"message":"Forbidden due to traffic filtering"}`, wrong credentials included. Only
the data plane is filtered: `/v1/search`, the dashboard and its console stay behind roll's
sign-in and work from anywhere. Branches start with their parent's list. How the client address
is decided (PROXY protocol, a trusted proxy's header, the fleet's signed forwards), and what an
owner must allow for roll apps and previews, is in architecture.md, "Client addresses and IP
allowlists". `crates/search-control/tests/control.rs` `ip_allowlists_turn_addresses_away_before_credentials`
and `tests/cluster.rs` `a_forward_carries_the_clients_address_to_the_owner` cover it.

**Search from the browser** (Algolia's search-only and secured API keys, Typesense's scoped keys,
Meilisearch's tenant tokens, Elastic Cloud's per-deployment `http.cors.*` and Elastic's search
applications). A web page calls a cluster directly, with no backend of its own in front:

- *Allowed origins.* A cluster's `cors` (`PATCH {"cors": {"allow_origin": [...], "allow_methods"?,
  "allow_headers"?, "max_age"?}}`, the dashboard's Settings → Browser search; `null` or no origins
  turns it off, the default) is Elasticsearch's `http.cors.*` with its semantics
  (`search_server::cors`): each origin `scheme://host[:port]` (kept as browsers send it: lower
  case, no default port), `/regex/` (matching the whole origin) or `*`, at most 32; methods,
  headers and max age default to Elasticsearch's (`OPTIONS,HEAD,GET,POST,PUT,DELETE`;
  `X-Requested-With,Content-Type,Content-Length,Authorization,Accept,User-Agent,X-Elastic-Client-Meta`;
  20 days). `allow-credentials` is always false: pages send keys in `Authorization`, never
  cookies. The cluster shows them in `_nodes/settings` and `_cluster/settings?include_defaults` as
  Elasticsearch shows `http.cors.*` node settings, changed live. Branches start with their
  parent's.
- *How requests are answered* (diffed against Elasticsearch 9.5.5 with `http.cors.enabled: true`):
  a preflight (`OPTIONS` with `Origin` and `Access-Control-Request-Method`) is answered before
  authentication, since browsers send none, and without waking a sleeping cluster: 200 with
  `Access-Control-Allow-Origin`, `Vary: origin`, one `Access-Control-Allow-Methods` line per method
  and one `Access-Control-Allow-Headers` line per header, and `Access-Control-Max-Age`; another
  origin gets an empty 403. Any other request is authenticated first; then an origin the cluster
  doesn't allow gets an empty 403 (wrong credentials get their 401, without CORS headers), and
  an allowed one gets `Access-Control-Allow-Origin`, `Vary: origin` and
  `Access-Control-Expose-Headers: X-elastic-product` on every answer, errors included, so a page
  can read a 401 or a 429. A request from the cluster's own host (`isSameOrigin`) passes. One
  difference: `*` is answered with `Vary: origin` too (Elasticsearch leaves it out), so a cache in
  front never hands one origin's answer to another. A cluster without CORS answers a preflight
  as any unauthenticated request (401).
- *A preflight names its cluster by host*: it has no credentials, so the cluster is the `Host`
  label under `--search-domain` (`<name>.<domain>`). A node without a domain serves every cluster
  at one address, and browsers can't call its clusters; the dashboard says so.
- *Allowed origins bind browsers, not keys.* Only a browser enforces CORS: curl, a server or a
  script sends a browser key from anywhere, within its rate limits. A key a page ships is public;
  what it may read, its filter and its limits are what protect the cluster.
- *The IP allowlist comes first*, preflights included: a cluster with an allowlist refuses
  browsers outside it with the traffic-filter 403 before anything else, so a public site needs
  its visitors' networks allowed (usually: no allowlist).
- *Browser keys* (`/v1/search/clusters/{id}/browser-keys`, Elasticsearch-flavor clusters): API
  keys of the cluster made through its own `_security/api_key`, so Elasticsearch's clients need
  nothing new. One role descriptor with `read` on named indices, or on one search application
  with `restriction.workflows: ["search_application_query"]` (the key runs only that
  application's template, its `params` checked against the template's `dictionary`); no cluster
  privilege, so no write, no `manage` and no `_security`. An optional `filter` is the role's
  document-level `query` (Algolia's secured-key filters), and `expires_in_s` its `expiration`.
  The encoded key (`base64(<key id>@<cluster>:<key>)`, `Authorization: ApiKey …`) is in the
  create answer only; the list shows what each reaches, its filter, expiry and limit, never a
  secret; revoking refuses any key that isn't a browser key (404); on a deleted cluster a
  browser key of it (its stored security says so) is kept for the restore, which revokes it
  before it answers (202). Names starting with `.` and a bare `*` are refused; a
  pattern reads every index whose name it matches, hidden ones (`index.hidden`) included, as an
  Elasticsearch role's pattern does. At most 20 browser keys per cluster (409 past it).
- *A browser key works only on its own cluster.* Its `metadata.roll_browser.cluster` is the
  cluster's id, and the node refuses it (401, as an unknown key) on any other: a branch copies its
  parent's security, keys included, and a point-in-time branch from before a revoke would bring a
  revoked key back. A restore keeps the cluster's id, so its keys keep working. A branch lists none
  of its parent's browser keys.
- *A point-in-time branch takes its parent's security as it is now*, whatever key: users and their
  passwords, API keys, tokens, service tokens, roles, mappings and profiles (OpenSearch Security's
  configuration on an OpenSearch-flavor cluster) are copied from the parent's latest state into
  the branch's before it is in the catalog, so every revocation, invalidation, deletion and
  password change made after `at` holds there; only the data goes back in time. A branch made now
  starts from the parent's security of now anyway; changes made to the parent after that stay the
  parent's. A point-in-time branch has the data as of `at` and today's security: a document
  deleted after `at` is back, and a role loosened since because that document was gone now reaches
  it. Review its roles before sharing it.
- *What a browser key may ask for* (`search_es::xpack::security::browser`; Algolia's search-only
  keys): it's a public credential, so it gets what search-as-you-type needs and nothing else.
  The policy (the key's `limits` over safe defaults) rides with the request's reads
  (`dls::ReadSecurity`) and is enforced where the engine has the request parsed, never on the
  raw request:
  - *APIs*, by the routed API's name: searches (`_search`, `_msearch`, `_count`), document reads,
    `_terms_enum`, a search application's `_search`, and the root and `_security/_authenticate`
    that clients call first. Any other API is a 400 naming why: no scrolls, points in time or
    stored async searches (they hold state on the cluster), no term vectors or `_explain`, no
    SQL, ES|QL or EQL, no search templates. The internal calls an allowed API makes for the key
    (`_terms_enum` reads `_field_caps`) pass as that API; a client can't mark its own.
  - *Queries*: every query the request builds (the query, `post_filter`, aggregations' filters,
    highlight and rescore queries, inner hits, collapse, a search application's rendered
    template), unless `limits.allow_expensive_queries`, gets the refusals of Elasticsearch's
    `search.allow_expensive_queries: false`: no `regexp`, `wildcard`, `prefix` (without
    `index_prefixes`) or `fuzzy` queries, joining queries (`nested`, `has_child`, `has_parent`),
    `percolate`, ranges on `text` or `keyword`, script queries, or queries on runtime fields.
    roll adds its own: no `query_string` (so no `?q=`: its syntax reaches regexps, wildcards and
    fuzziness), intervals' `prefix`, `wildcard`, `regexp` or `fuzzy` rules, or `span_multi`
    over them. Elasticsearch's setting leaves alone what search-as-you-type needs, and so does a
    browser key: a `match` with `fuzziness` and `match_phrase_prefix`, `match_bool_prefix` and
    `multi_match` (`bool_prefix` included), whose expansions stop at `max_expansions` (50);
    `simple_query_string` (its `*` and `~` operators included); `search_as_you_type` fields and
    the completion suggester. A search application's template a browser key runs is held to the
    same list: write it with those (Elastic's default template uses `query_string`, so a browser
    key can't run it). The owner's own filters (the key's `filter`, an alias's filter) aren't
    held to the list: they're the owner's, as an Algolia secured key's filters are.
  - *Painless*: none compiles for the key unless `limits.allow_scripts`: script queries and
    scores, `script_fields`, `runtime_mappings` and the mapping's runtime fields, scripted sorts,
    aggregations and pipelines, highlighters over runtime fields.
  - *The search source*, URL and body merged, every `_msearch` item included, and again for
    every search the request runs (collapse's `inner_hits` expansions, retrievers' children): no
    `explain` (at any level: `top_hits` and inner hits too) or `profile`, no `pit` or scroll;
    `from + size` at most `limits.max_result_window` (1,000).
  - *One fetch budget per request*: every document the request fetches (hits, `top_hits` in
    every bucket, inner hits; an `_msearch`'s items share it) counts against
    `limits.max_result_window`, past it 400; so does each search collapse's `inner_hits` expand
    (one per definition per page hit, `size: 0` ones too), at most 100 a request; aggregations
    stop at `limits.max_buckets` (1,000). `significant_text` re-analyzes the `_source` of every
    document it collects (every match, as in Elasticsearch, which suggests a `sampler` above
    it), and no limit above bounds that: a browser key's `significant_text` over a broad query
    costs that much text analysis per request, so the page's queries should put it under a
    `sampler`.
  - *Inference* (queries on `semantic_text` fields, `semantic`, `sparse_vector` and `knn` that
    embed a text, `text_similarity_reranker`): a browser key embeds only with the endpoints its
    target indices' `semantic_text` fields search with, plus those `limits.inference_endpoints`
    names; an explicit `inference_id` or `model_id` naming any other is a 400. At most
    `limits.max_inference` (4) distinct texts a request (every query and `_msearch` item
    together, a reranker counting one), each at most 1,000 characters (a reranker's
    `inference_text` too), each charged one item to the rate limits (below) before its call. A
    refused charge gives nothing back: what the request already ran (its admission, other
    `_msearch` items) stays paid. A reranker needs its endpoint in `inference_endpoints` and a `rank_window_size` of
    at most `limits.max_rerank_window` (100), and is charged one item per 10 documents of its
    window. For every caller, an endpoint whose task type can't serve the query (a
    `text_embedding` one named by `sparse_vector`) is refused before the call.
  - *`_terms_enum`*: `size` at most `limits.max_result_window` and `timeout` at most 5s
    (Elasticsearch caps no one); the `timeout` holds during the walk for everyone, as in
    Elasticsearch (the aggregation path for `constant_keyword` and `wildcard` too), past it the
    terms found so far with `complete: false`. An index whose documents the key's `filter` restricts
    takes no part (as for any reader with document level security).
  - *Items*: an `_msearch`'s searches and an `_mget`'s documents, as the engine parsed them
    (whatever the URL, `?source=` or body format), are charged to the key's rate limits before
    they run (below); more than `limits.max_items` (100) is a 429. An `_msearch` is counted from
    its lines first (a header and a body line per search): past the limit it's refused before
    any line is parsed, and its admission token goes back. Once a body was parsed, a refused
    request keeps its admission token as payment.
  - *Body*: at most `limits.max_body_bytes` (1 MiB), under the cluster's own `max_body_bytes`:
    one token never buys a parse of the cluster's whole body limit.

  The owner widens them in `limits` when making the key (`max_concurrent` 1-64,
  `max_result_window` up to 10,000, `max_buckets` up to 65,536, `max_items` up to 1,000,
  `max_inference` up to 64, `max_rerank_window` up to 1,000, `max_body_bytes` up to 100 MiB,
  `inference_endpoints` up to 16 ids, the two `allow_` switches); the list shows them.
- *Term statistics aren't filtered* (as in Elasticsearch): a filter (document level security)
  hides documents from searches and gets, and exact counts, but `_score` (idf, average field
  lengths), term vectors' `term_statistics` and `_explain` come from every document of the
  shard, so a user or API key with a filter can learn about hidden documents through them. A
  browser key can't ask for term vectors or `_explain`; its scores still reflect every
  document.
- *Rate limits* (Algolia's `maxQueriesPerIPPerHour`): per key `rate_limit: {per_minute?,
  per_ip_per_minute?}` (defaults 600 and 60 a minute, at most 100,000), kept in the key's
  `metadata.roll_browser`, so they travel with the cluster; a key's limit is shared by all its
  visitors, and the per-address one keeps a single visitor from using it up. Per cluster,
  `browser_per_minute` (`PATCH`; `null`: 6,000) is a ceiling every browser key draws from. The
  cluster's owner node charges each request a browser key authenticates one token per item it
  holds (an `_msearch` of 50 searches, an `_mget` of 50 documents: 50; inference above adds its
  items) to every bucket, all or none: the cluster's, the key's, and the key's per client address at three widths (the address,
  an IPv6 /64 counting as one; its /56 at 4 times and its /48 at 16 times the per-address limit;
  IPv4 /32, /24 and /16), the wider two never past a quarter of the key's or the cluster's limit,
  so one network can't take more than a quarter of either. An empty bucket answers 429 `es_rejected_execution_exception` (as Elasticsearch
  rejects work it can't take) with `Retry-After` in seconds (exposed to the page through `Access-Control-Expose-Headers`); a refused request takes nothing from
  the other buckets. The buckets are in memory on the owner and bounded (50,000 addresses per
  cluster; past it the fullest are forgotten first, so a drained address keeps its debt): a takeover, a drain or an unload starts them full again.
  roll's own requests (the console) aren't counted.
- *Running at once*: browser requests are capped per key (`limits.max_concurrent`, 4) and per
  cluster (a quarter of the cluster's `max_in_flight`, at least 4 but never more than half, so
  the owner's own clients always find a permit; a cluster of one permit runs no browser
  request). Past either: 429 with `Retry-After: 1`, and the request's token goes back.
- *The safe surface* is `_application/search_application/{name}/_search` with a key limited to
  that application: a page sends only the template's `params`. Search UI's
  `@elastic/search-ui-elasticsearch-connector` and InstantSearch through Searchkit send whole
  queries to `{index}/_search` and `_msearch`, so they take a key that reads indices (an
  application's name is an alias of its indices, so a key reading that alias works for them),
  ideally with a `filter`. The dashboard's quickstart has all three filled in with the cluster's
  endpoint.
- *Billing*: a public key means public traffic. Browser requests are metered like any other, and
  a sleeping cluster wakes for them; each key's rate limit is what bounds them.

`crates/search-server/tests/http.rs` `cors_preflights_are_answered_before_authentication`,
`crates/search-control/tests/control.rs` `browsers_search_with_browser_keys` and
`tests/cluster.rs` `every_node_answers_a_preflight_the_same_way` cover it; an end-to-end run with
Searchkit and InstantSearch in Chromium is in the review pack (`94-browser-*`).

**Cross-cluster search** (Elastic Cloud's remote clusters between deployments, Elasticsearch 9's
API key based remote cluster security; `search-control` `remotes.rs`, `search-es`
`remote/links.rs`). A cluster may name another roll search cluster as a remote, and search it as
`<alias>:<index>`, when both have the same owner: one person's, or both the operator's (admins
act for the owner, they can't link two owners' clusters). Never across owners: names are global,
so a remote naming another person's cluster is refused exactly as one naming a cluster that
doesn't exist (404 from the route, the same 400 from `PUT _cluster/settings`). Both must be
Elasticsearch-flavor.

- **Linking**: `PUT /v1/search/clusters/{id}/remotes/{alias}` `{cluster, skip_unavailable?,
  access?}` makes a `cross_cluster` API key on the remote with `access` (Elasticsearch's shape,
  `{"search": [{"names": ["logs-*"], "query"?, "field_security"?}]}`; default every index), keeps
  it as `cluster.remote.<alias>.credentials` and sets `cluster.remote.<alias>.mode: proxy` and
  `proxy_address: <remote name>`. Or `{cluster, api_key}` with a key the person made on the remote
  (`POST _security/cross_cluster/api_key`, its `encoded` value), checked there first. The route is
  the way in because roll clusters have no keystore a tenant writes, and it needs no endpoint
  syntax; Elasticsearch's own `PUT _cluster/settings` with `mode: proxy` and `proxy_address` (or
  `seeds`) naming a cluster (its name, or its endpoint `<name>.<domain>[:port]`) works too, held
  to the same owner rule, but connects only once the remote has credentials.
- **Credentials** are a secure setting: sealed under `ROLL_SECRET_KEY` and bound to their row
  (cluster, alias, remote) in the catalog (`search_remotes`), never returned (`_remote/info`
  shows `cluster_credentials: "::es_redacted::"`). The remote authorizes each request with the
  key's access intersected with the local user's `remote_indices` privileges for the alias (the
  cluster's own password is a superuser: everything the key allows), as Elasticsearch does.
  Revoking the key on the remote (`DELETE _security/api_key`) cuts the link; `DELETE …/remotes/
  {alias}` removes the settings and credentials and revokes a key roll made, and so does deleting
  the cluster for good (at once with no retention, else when its restore window ends). A cluster
  holds credentials for at most 20 remotes (409 past that). Without `ROLL_SECRET_KEY` (local
  development) credentials are stored as given, as every secret search keeps.
- **Execution**: the remote's part runs on the remote cluster's owner node through roll's own
  in-process call or signed fleet forward (`Control::on_owner`, `cluster::to_owner`), never over
  the public endpoint, so neither cluster's IP allowlist applies to it: a client the local
  cluster's allowlist admits reaches the remote's data within the key's access even when the
  remote's own allowlist wouldn't admit that client directly. Link only clusters whose data may
  be read from wherever the local one may be. The link's header
  (`x-roll-remote-access`) is honored only on those calls; a client sending it, or presenting a
  cross-cluster key, gets 401. A sleeping remote wakes for it, for at most
  `cluster.remote.initial_connect_timeout` (30 s); one that can't be reached, or answers an error
  (a revoked key, an index outside the key's access), is skipped with its failure in `_clusters`
  when `skip_unavailable` (the default) and fails the search otherwise.
- **What reaches remotes**: `_search` and `_msearch` (merged as `ccs_minimize_roundtrips` does:
  hits by score or sort values, totals, shards and failures, a `_clusters` section; remotes'
  answers are held on the request circuit breaker while they merge), `_field_caps` (each remote
  index's capabilities merged as local ones are), `_resolve/index`, `_resolve/cluster` and
  `_remote/info`. An expression is authorized on the remotes only when every remote its alias
  pattern matches is a link; `*:index` over a link and a loopback remote is authorized locally,
  whole.
- **Aggregations**: every aggregation, pipelines included, answers what one cluster holding all
  the data would. Each cluster reduces its part partially and the local cluster does the final
  reduce from their internal state (`search-aggs` `wire`): HyperLogLog sketches, t-digests and
  HDR histograms, sums with their counts, top hits with their sort keys (fetched on their own
  cluster), held to `search.max_buckets` by the final reduce as one cluster's are. The state is
  held on the request breaker before it's decoded, its nesting is bounded
  (`search_aggs::wire::MAX_NESTING`: a document's 1000 JSON levels under 100 aggregation
  levels), and it's encoded and decoded on a thread with a stack of its own. Only a
  link's own request may ask for that state (`x-roll-partial-aggs`, honored
  for a cross-cluster key's request only); a client asking gets the public answer. A
  `geo_centroid`'s sums add up in another order, so its last digit may differ from one
  cluster's, as Elasticsearch's does.
- **Scroll and point in time** (`search-es` `remote/contexts.rs`): one opaque id holds each
  cluster's own scroll or point in time, kept on the local cluster with the caller that opened
  it. It answers only there and only to that caller (anyone else gets
  `search_context_missing_exception`, 404), and the parts' ids never leave roll. Each part keeps
  its own keep-alive, sent along with every page. A scroll page is the `size` best hits left
  over all clusters (the coordinator keeps what a part sent ahead and asks it for more only when
  it runs short), so pages go through the documents exactly as one cluster's scroll does.
  `_search/scroll`, `DELETE _search/scroll`, `DELETE _pit` and point-in-time searches fan out
  to the parts. The hits a part sent ahead are held on the request breaker for as long as the
  scroll lives. A part with nothing to fetch is kept alive as long as the scroll through the
  link's keep-alive call, which fetches nothing; a point in time is extended only by a search
  that sends `keep_alive`. Pages of one scroll run one at a time, and a clear or close waits for
  the page in flight. Every search on a point in time is authorized for its caller against the
  point in time's local indices (a role that loses one loses it there too), and one naming
  indices in its path is refused (400) before anything reads them. A scroll or point in time of
  one cluster is its opener's alone too. `search_after` with a point in time pages exactly: each hit's `_shard_doc`
  tiebreaker carries its cluster above its shard, and every cluster continues from the last
  hit's place in that order.
- **OpenSearch-flavor clusters**: `cluster.remote.*` and `_remote/info` answer as OpenSearch
  3.9 does (`skip_unavailable` defaults to `false`, a seed or proxy address must be
  `host:port`, no `cluster_credentials`), and a remote naming a roll cluster goes through the
  same owner check. Linking them is left: OpenSearch has no `cross_cluster` API keys, so links
  between OpenSearch-flavor clusters need a credential model of their own (probably an
  internal user on the remote with a search-only role, issued and sealed as links' keys are).
  Until then a search naming `alias:index` on them is refused (400), loopback remotes included,
  where OpenSearch searches a loopback.
- **ES|QL** (`search-es` `xpack/esql/ccs.rs`): `FROM <alias>:<index>` (`*:logs-*`,
  `local,remote:idx`) answers as one cluster holding all the data would, as Elasticsearch 9.5.5
  answers it. Each cluster runs its **part**: the source, the commands that work row by row
  (`WHERE`, `EVAL`, `KEEP`, `DISSECT`, `MV_EXPAND`, `ENRICH` in `_remote` and `_any` mode,
  `LOOKUP JOIN`...) and its first breaker's own part: `STATS` reduced partially (each group's
  accumulator states: sums with their compensation, HyperLogLog++ sketches, collected values,
  merged here as one cluster's rows would have been, over just the columns it reads),
  `SORT ... | LIMIT n` (with `EVAL`, `KEEP`, `DROP`, `RENAME` and `ENRICH` between, the parts
  carrying their sort keys' values) or `LIMIT n` applied there and again on the coordinating
  cluster, and an `ENRICH _remote` after the limit (with the commands before it) still in the
  parts; a command reading each cluster's own policy or lookup index never runs on the
  coordinating cluster: it's in the parts or refused with Elasticsearch's error; a `LIMIT` (the implicit one too) also runs in the
  parts below the commands it commutes with (`ENRICH` in any mode, `EVAL`...), and a part with
  nothing to bound it stops at what the coordinating cluster's request breaker can hold (a
  `circuit_breaking_exception`). What follows runs on the coordinating
  cluster over every part's result, and so does a breaker with no part of its own (a `SORT`
  without a `LIMIT` the parts can run, `INLINE STATS`, `ENRICH _coordinator`), over the parts'
  rows. Every cluster's part reads the
  same columns: the source resolved once across clusters, a field one cluster doesn't map
  reading null there. `METADATA _index` names remote indices `alias:index`, `ENRICH _remote`
  uses each cluster's own policy, `_coordinator` the local one, `_any` each cluster's own before
  the first breaker and the local one after it; `ENRICH _remote` after `STATS`, `FORK` or
  `ENRICH _coordinator`, and `LOOKUP JOIN` after `STATS`, `FORK` or `LIMIT`, are refused with
  Elasticsearch's errors (naming the command the parts stopped at), and so are a policy missing
  on a cluster that needs it (`cannot find enrich policy [p] on clusters [_local, far]`) and a
  policy whose enrich fields differ between clusters; a lookup index's field the clusters map
  with different types is a union type. The mode (`_REMOTE:p` too) is read in any case. A remote's `ENRICH` needs `monitor_enrich` there
  (`remote_cluster` on the caller's role, carried over the link as the remote's cluster
  privileges; 403 without), after which the remote reads its own policy's index, as
  Elasticsearch's does. An exclusion of a remote's (`-far:x`, `far:-x`) is that remote's own;
  a remote's ES|QL view is refused (`remote_view_not_supported_exception`). The parts' commands
  are compiled on the coordinating cluster first, so the query's own errors (`Unknown column`)
  are the query's; only a remote that can't be reached or authenticate, or whose indices are
  missing (or outside the link's access), is skipped. A remote decides what its part may run from
  the query it parsed, never from what the request says about it: the shapes the coordinating
  cluster cuts, Elasticsearch's placement rules, and every `ENRICH` it runs checked for
  `monitor_enrich` and its policy; the coordinating cluster parses each part's text as its
  cluster will before sending it, and a `METADATA` name outside the metadata fields (a quoted
  one always is) is Elasticsearch's verification error. A caller with no `remote_indices` for a
  remote is refused (403) on a query over remotes alone, and skips the remote otherwise. `include_ccs_metadata` (or
  `include_execution_metadata`) adds `_clusters`; a skipped cluster makes `is_partial` true; a
  missing remote is `no_such_remote_cluster_exception` (404), and a remote's missing index or any
  failure of its part skips it (`skip_unavailable`) or fails the query. `_query/async` answers
  the same. A remote's part is the query's own text with its `FROM` naming that cluster's indices
  and the commands it doesn't run blanked, so the positions in its errors are the query's; it
  runs as a `_query` on the remote's owner through the link (`x-roll-esql-part` marks it, honored
  only for a link's own request; the part itself, its columns included, travels in the body),
  authorized there exactly as a `_search` part is: the key's access,
  its document and field level security included, intersected with the caller's
  `remote_indices`. Its answer (typed rows, or `STATS` states) is held on the request breaker
  before it's parsed (its bytes and, per value it may hold, what that costs decoded) and decoded
  with one level of multi-values, every row exactly as wide as its columns and every value of its
  column's type, `STATS` states of the kinds and over the columns the coordinating cluster's own
  compilation makes, sketches checked (`Hll::from_parts`), never sized from a count it sent. A
  `STATS` part that can't answer its states fails rather than answering nothing. Remote parts
  still running when the query ends (an error, a client gone) are aborted. Queries the
  coordinating cluster doesn't cut into parts (`FORK`, subqueries, views, `TS`, `PROMQL`,
  `SET unmapped_fields`) read the remotes' rows through cross-cluster search there, without
  `_clusters`; in them `LOOKUP JOIN`, `ENRICH _remote`, and `ENRICH` in `_any` mode before any
  `LIMIT`, `STATS`, `FORK` or `ENRICH _coordinator` are refused (Elasticsearch's placement
  error where it has one, each `FORK` branch checked after the commands before it), never run
  with the coordinating cluster's own policy or lookup index.
- **Left**: suggesters over several clusters.

`crates/search-control/tests/control.rs` `cross_cluster_search_between_a_persons_own_clusters`,
`a_sleeping_remote_wakes_and_a_gone_one_is_skipped`, `aggregations_across_clusters_equal_one_clusters`,
`scroll_and_point_in_time_across_clusters`, `esql_across_clusters_answers_as_one_cluster` and
`esql_across_clusters_reads_as_the_link_allows`, and `tests/cluster.rs`
`a_remote_on_another_node_is_searched_through_the_signed_forward` and
`a_scroll_over_clusters_on_two_nodes_pages_through_either_front` cover it. The dashboard lists a
cluster's remotes in its Settings (alias, cluster, `skip_unavailable`, state), and links or
unlinks one of the person's own clusters (`components/search/remotes.tsx`).

**Migrating into roll** (Elastic Cloud's restore from another deployment, AWS OpenSearch
Service's migration assistant; `search-control` `migrate.rs`, `search-es` `api/remote.rs`). The
way in from Elastic Cloud, a self-hosted Elasticsearch or OpenSearch is reindex from remote, as
Elasticsearch 9.5.5 does it from the client's side: `source.remote.host` over `https://` (or
`http://`), `username`/`password` or `headers` (`Authorization: ApiKey …`), `socket_timeout`,
`connect_timeout`, `query`, `size`, `max_docs`, `_source` filtering, `requests_per_second`,
`wait_for_completion=false` with `_tasks`, `_rethrottle` and cancel; `slices` refused as
Elasticsearch refuses it for a remote. A remote of 7.10 or later is read through a point in time
with `search_after`, an older one (and OpenSearch) with a scroll, one page held at a time and
written before the next is read; the search context is released however the run ends.

A remote must be in the cluster's `reindex.remote.whitelist`, as in Elasticsearch: refused
unless whitelisted. It's a per-cluster setting the cluster's owner sets through roll's API
(`PATCH /v1/search/clusters/{id}` `{"reindex_whitelist": ["es.example.com:443"]}`, `host:port`
patterns with `*`, at most 64), as Elastic Cloud's user settings set it; roll's migration adds
exactly the host it was given. A node setting of the same name applies to clusters without
their own (`search-dev --yaml-test-cluster` sets `127.0.0.1:*`).

The remote is a URL a tenant chose, so every request goes through the guarded client, and the
node's operator rules apply on top of the whitelist: names resolve to public addresses only
(`roll_net::PublicOnly`), private literals and `localhost` are refused (rolld's
`--search-reindex-allow-private`, `ROLL_SEARCH_REINDEX_ALLOW_PRIVATE`, lifts both, for local
development), `reindex.remote.blocklist` refuses what it matches (rolld adds its own endpoints:
its public host and its clusters' domain, matched in lower case without a trailing dot), no
redirect is followed and no proxy is used. The lists match the host exactly as the client connects
to it (percent-decoded, IDNA-mapped, lower case, no trailing dot; a host that would move the
port or the path, with `?`, `#` or `\` in it, is refused), and `source.remote.headers` can't set
`Host` or a pseudo-header. Names alone can't keep a remote off roll, so rolld also denies
its endpoints by address: the names it serves on (its public host, the clusters' domain, a name
under the domain's wildcard, `public_url`'s host: `roll.reindex.remote.deny_names`) are resolved
again at most every 30 seconds (a name that fails to resolve keeps the addresses it had and is
asked again within 5 seconds; until each has answered once, with an address or "no such name"
(NXDOMAIN, or no address records: a resolver that fails or times out isn't an answer), no
connection goes out) and checked on the address each connection uses and on an address
written in the URL, and the fleet's
networks given with `--search-reindex-deny` (`ROLL_SEARCH_REINDEX_DENY`, in
`roll.reindex.remote.deny_addresses`) and those names' addresses at boot are checked on literals in
any form, IPv4 inside IPv6 (mapped, NAT64, 6to4) included. HTTPS is
verified against the platform's roots (plus `reindex.ssl.certificate_authorities`: rolld's
`--search-reindex-ca`, `ROLL_SEARCH_REINDEX_CA`, for a remote with a private CA).

What one tenant can make a node spend is bounded by the operator, never by the tenant: at most 2
remote reindex tasks per cluster and 4 per node run at once (`roll.reindex.remote.max_per_cluster`,
`roll.reindex.remote.max_per_node`; past them a 429 `es_rejected_execution_exception`); each
answer is read to at most 16 MiB (`roll.reindex.remote.buffer_limit`; past it Elasticsearch's
"Remote responded with a chunk that was too large. Use a smaller batch size.") and charged to a
node-wide budget (`roll.reindex.remote.memory`, 256 MiB) while it's held. A page of hits is never
parsed into a tree: it's scanned once, only its envelope (ids, `_index`, `_id`, `_routing`, sort
values, `_shards`) is decoded, and each `_source` goes to indexing as a slice of the page's bytes;
a small answer that is parsed (`GET /`, a definition) is charged for its tree before it's built; a scroll or point-in-time id is
kept up to 4 KiB; each read times out, a request's whole exchange stops at 10 minutes, and a task
stops after 24 hours (`--search-reindex-max-duration`, `ROLL_SEARCH_REINDEX_MAX_DURATION`; it ends
with a `canceled` reason that says so). A cluster with a running task isn't unloaded to make
room in memory, and a cluster unloaded on purpose (deleted, handed over, put to sleep) cancels its
tasks first, so `_tasks` says what happened; a task lost with its node (a crash) is reported by
the dashboard as stopped, its copy partial.

The remote's answers are untrusted input. Credentials never leave the client but on the wire: a
task's description names the host alone (`reindex from [scheme=https host=… port=…][logs] to
[logs]`, as Elasticsearch prints it); an error never quotes what the remote answered to a request
that carried credentials (it gives the status and Elasticsearch's error shape, without the body,
since a remote could echo a credential in any encoding); and a transport failure has the password,
the `Authorization` value and every header value replaced by `::es_redacted::`. The dashboard's
console keeps no `Authorization` header in its history.

A warning for whoever runs the remote: allowlisting roll's egress addresses in a firewall opens it
to every roll tenant whose cluster whitelists that host, not to one tenant. Require credentials
on the remote (an API key per migration), and remove the key when the migration is done.

The dashboard's Indices tab has **Migrate**: the remote's URL and an API key (it needs the
`monitor` cluster privilege and `read` and `view_index_metadata` on the indices) or a user and
password, sent to roll and never shown back. roll lists the remote's open indices with documents,
size and aliases, marking the aliases roll already has (hidden and system indices aside; data
streams named but not copied yet). For
each picked index roll creates the copy (its own name unless roll has one) with the remote's
mappings, settings and aliases (an alias roll already has is never joined: it's listed with the
dropped settings, "alias exists on roll"), dropping the settings it doesn't take and saying why: those the
remote's cluster set (uuid, creation date, versions), shard allocation, blocks (they'd stop the
copy), lifecycle policies (not copied), storage, a pipeline roll doesn't have, and any setting
roll's index creation refuses (its error names it). Then it starts the reindex task, which the
panel follows through `_tasks` with cancel; a copy under another name can take the remote's name
as an alias when it's done. Copies keep running if the panel closes. A copy that failed, was
cancelled or was lost (`_tasks` answers 404: the cluster restarted or moved) is shown as stopped,
its copy partial, with a button that deletes the partial index so it can be copied again.

| method | path | |
| ------ | ---- | - |
| POST | `/v1/search/clusters/{id}/migrate/inspect` | `{source: {host, api_key? \| username?, password?}}` → `{remote: {host, version, distribution, cluster_name}, indices: [{index, health, status, docs, size_bytes, primary_size_bytes, primaries, replicas, aliases, aliases_more, aliases_on_roll, exists}], more_indices, data_streams, more_data_streams}` within 60s (`primaries`, `replicas` and `primary_size_bytes` are `null`: the listing asks only for the columns it shows, keeps only those, reads it row by row and lists the first 10,000 indices, with `more_indices` saying how many more there are; each index carries at most 20 alias names, `aliases_more` counting the rest, and at most 100 data streams are named; a listing that names one index twice is refused; everything the answer holds is charged to the control budget until it's sent, 64 KiB at a time; an answer not read within 60s is given back and its connection closed); a remote's refusal is a 400 with its status and `error.type` only (never its body), and every transport failure reads the same (`couldn't reach the remote at <host>`). The operator's rules apply, not the cluster's whitelist. One `inspect` or `migrate` runs per cluster at a time, and 4 per node (429 past them); their answers are capped (1 MiB, an index definition 4 MiB) and drawn from a slice of the node's reindex budget that tasks never use. `write` for roll-access |
| POST | `/v1/search/clusters/{id}/migrate` | `{source, indices: [{index, dest?}], requests_per_second?, size?}` (at most 100 indices) → `{indices: [{index, dest, task, dropped: [{setting, value, reason}], aliases} \| {index, dest, error}]}`: the remote must answer (`GET /`) before its `host:port` is added to the cluster's `reindex_whitelist` (exactly that host: `normalize_host` refuses anything that isn't a DNS name or an IP literal, so no pattern gets in); then, while a reindex task slot is free, each index's copy is created and its `wait_for_completion=false` reindex started, within 60s; an index past the free slots comes back `{index, dest, queued: true, reason}` with nothing created (the dashboard starts it as earlier copies end); each row says what happened (`created`, `task`), also when the deadline cut it short, and a copy an earlier call already started counts as started; one index's failure doesn't stop the others; an index roll already has is refused, not overwritten, and an alias roll already has is dropped (`aliases.<name>`). `admin` for roll-access: it changes the cluster's whitelist and starts copies as the cluster's own user |

`crates/search-es/tests/reindex_remote.rs` covers the guard against a hostile mock remote (only
whitelisted hosts read; a redirect not followed; an oversized page refused, with and without a
`Content-Length`, the node's budget back to zero after; private hosts refused without the flag,
also behind a name that resolves to one; a third task of a cluster refused with a 429 until one
ends; a remote's answer never quoted once credentials were sent, whatever encoding it echoes them
in) and, with `ROLL_REINDEX_ES` set, a real Elasticsearch 9.5.5 over HTTPS (a query, an API key,
a wrong password answering in Elasticsearch's own shape, a cancelled task, every point in time
released). `crates/search-control/tests/control.rs` `migrating_from_another_cluster` runs the
routes' flow (an alias roll has left alone, the whitelist gaining exactly the remote's host,
another host refused); the dashboard's logic is in `apps/dashboard/src/lib/search-migrate.ts`
(tested beside it, a lost task included).

Phase 2, Algolia (planned, not built): an import that reads an Algolia application through its
REST API with an admin key the caller gives once (the same guarded client and sealing rules):
`browse` every record of the picked indices into roll indices (`objectID` as `_id`, with
mappings inferred from the first pages as the file import does, and Algolia's searchable
attributes, attributes for faceting and custom ranking turned into `text`, `keyword` and numeric
fields with `index_options` to match), its synonyms (`synonym`, `onewaysynonym`, `altcorrection`,
`placeholder`) into roll synonym sets, its rules (conditions on a query pattern, consequences that
promote, hide or filter) into query rules where Elasticsearch's rule criteria can say the same and
listed as dropped where they can't, and its replicas into sorts the dashboard records for the
browser keys. Progress, cancel and the alias switch as above.

**Terraform** (Elastic Cloud's `ec_deployment` and `elasticstack` providers, AWS's
`aws_opensearch_domain`). roll's provider (`sdk/terraform-provider-roll`, its README has the
details) has `roll_search_cluster` (name, flavor, `always_on` or `idle_timeout_s`, limits,
`ip_allowlist`; computed endpoint, status, version and the password from create; a new name
makes a new cluster, a new flavor switches it in place keeping its data (links made before are
reissued, and keys don't carry over: an API key isn't an OpenSearch user); import by id or name; destroy deletes it, restorable for
`deleted_retention_s`, 7 days by default), `roll_search_api_key` (`access`, or Elasticsearch
`role_descriptors` through the cluster's `_security/api_key`, and an `expiration`; its secret is
sensitive, replaced on any change and after a flavor switch, revoked on destroy; also an ephemeral resource, revoked when
the run ends), `roll_search_snapshot_repository` (`s3`, `gcs` or `azure` through
`PUT _snapshot/<name>`, credentials in a sensitive map roll seals apart and never returns) and
the data source `roll_search_cluster` (by id or name). Reading keys and repositories never
wakes a sleeping cluster. There's no `deletion_protection` (roll has no setting to enforce it;
`prevent_destroy` covers the plan). `TestSearchAsCode`, `TestSearchClusterCap` and
`TestSearchKeyEphemeral` (`internal/provider/search_test.go`) drive Terraform against a fake
`/v1/search`: create, change in place, a flavor switch in place (the id kept), import, destroy, the cap's 409, and keys'
secrets kept out of the plan and the state. Left: remote-cluster links (#347) and allowed
origins (browser search), once they're on main.

| method | path | |
| ------ | ---- | - |
| GET | `/v1/search` | version, endpoint, defaults, pricing; for a person their `caps`; for the operator and admins also `storage`, `loaded` clusters and the `cache` |
| GET | `/v1/search/clusters` | list (a person: their own); `?deleted=true`: recently deleted clusters kept for restoring, newest first, with `deleted_at` and `purge_at` (Unix seconds), the name each had and its last figures (`stored_bytes` is still billed) |
| POST | `/v1/search/clusters` | create `{name, flavor?, parent?, at?, idle_timeout_s?, max_in_flight?, max_body_mb?, destructive_requires_name?}`; answers with `url`, `username`, `password` (once) and `id` — the shape `search-yaml run --cluster-hook` expects; `owner` is the signed-in person who isn't an admin, and a branch's is its parent's |
| GET | `/v1/search/clusters/{id or name}` | settings, `status` (`loaded` with live index count and requests in flight, and live `docs` once every index is open, or `asleep` with `wakes_at`, unix seconds, when a background job is due to wake it), last metered `indices`/`docs`/`stored_bytes` and `own_bytes` (what a branch references that its parent's latest commit points don't; all of `stored_bytes` for a cluster that isn't a branch), `url`, `branches` (each `{id, name, parent_at, created_at}`); a branch's `parent_id`, `parent_name` and `parent_at` (Unix seconds: the parent's state it started from). roll previews compares `parent_at` with `_cat/indices`' `creation.date` (Unix milliseconds) to leave out indices the parent made after the branch, so both fields stay |
| PATCH | `/v1/search/clusters/{id}` | `flavor`, `idle_timeout_s` (0: always loaded), `max_in_flight`, `max_body_mb`, `destructive_requires_name`, `ip_allowlist` (below), `cors` and `browser_per_minute` (above: browser search), `reindex_whitelist` (`host:port` patterns reindex from remote may read, "Migrating into roll"); the idle timeout, body limit, `max_in_flight`, allowlist and CORS apply at once (also to a cluster that is waking up); a new flavor or `destructive_requires_name` needs a reload: the cluster is put to sleep now, or as soon as no request runs (always-on clusters included), and keeps its current ones until then |
| DELETE | `/v1/search/clusters/{id}` | waits for running requests and for a wake in progress (which is locked out and unloaded, never left serving); the cluster leaves the list, its REST endpoint answers as for a missing cluster and its name is free at once, but its storage is kept for `--search-deleted-retention-days` (7) and billed as storage, for `restore` (0 days: removed at once); branches keep working |
| POST | `/v1/search/clusters/{id}/restore` | a deleted cluster (by its id) back as it was within its window: data, snapshots, settings, owner, password and API keys; under its own name, else 409 and `{"name": "…"}` restores it under another (the new name is its Basic auth user, API key id suffix and host label; the password stays). Scoped like every per-cluster route (owner, admins, the operator); 404 once purged |
| GET, POST | `/v1/search/clusters/{id}/branches` | list; branch `{name, at?}` (`at`: Unix seconds within `--search-history`) |
| POST | `/v1/search/clusters/{id}/password` | rotate; the new password is in the answer, once. From the answer on, the old one fails, also on a cluster that was waking up |
| POST | `/v1/search/clusters/{id}/unload` | sleep now (`{"unloaded": false}` while requests run) |
| GET | `/v1/search/clusters/{id}/indices` | `_cat/indices` as JSON numbers: health, status, shards, docs, sizes; docs and sizes of open indices come from the engine (as `_stats` reports them: Lucene documents, nested ones included, unflushed writes included). Wakes the cluster |
| POST | `/v1/search/clusters/{id}/request` | console: `{method, path, body?}` (a JSON body, sent byte for byte so `_source` keeps its key order, or a string such as NDJSON, sent as is) → `{status, headers, body, took_ms, truncated}`. Accepts bodies up to the cluster's `max_body_mb` (the request may be up to twice that plus 1 MiB, for JSON escapes) |
| GET | `/v1/search/clusters/{id}/usage?days=30` | loaded seconds, GiB-hours, requests, stored GiB-months, cost, and the same workload on Elastic Cloud Hosted, an Amazon OpenSearch Service domain and OpenSearch Serverless |
| GET | `/v1/search/clusters/{id}/metrics?range=1h\|24h\|14d` | request metrics (architecture.md, "Observability"): `{range, step_s, from, to, points, totals}`; each point (`at`, the start of its step: a minute for `1h`, 15 minutes for `24h`, 2 hours for `14d`; only steps with requests) and `totals` hold `all`, `search`, `index` and `other`, each `{requests, rate, 2xx, 4xx, 5xx, rejected, p50_ms, p95_ms, p99_ms}` (`rate` per second; `rejected`: 429s; percentiles `null` when nothing ran). Includes what the cluster's node recorded since the last metering pass |
| GET | `/v1/search/clusters/{id}/slowlog` | the search and indexing slow logs, newest first, at most 500: `{entries, max_entries}`, each entry Elasticsearch's JSON slow log line (`@timestamp`, `log.level`, `log.logger`, `elasticsearch.index.name`, `elasticsearch.shard.id`, `elasticsearch.slowlog.{message,took,took_millis,source,total_hits,stats,search_type,total_shards,id,routing}`) |
| GET | `/v1/search/clusters/{id}/audit?from=&to=&event=&size=` | the cluster's security audit log (below), newest first: `{flavor, retention_days, entries: [{at_ms, event, entry}], more}`. `from` (inclusive) and `to` (exclusive) are Unix milliseconds; `event` takes names or categories, comma-separated (an Elasticsearch `event.action` such as `put_user` too); `size` up to 1,000 (100). Never wakes the cluster. A read in roll's permissions, not a reveal: entries hold no secret |
| POST | `/v1/search/clusters/{id}/keys` | a new API key of the cluster `{name, expires_in_s?, access?}` (`access`: `admin`, the default, or `data`: a link's role, data, templates, pipelines and lifecycle policies without security) → `{id, name, access, api_key, url, expires_at?}` (`access`: the access the key was made with): `api_key` is `base64(<key id>@<cluster>:<key>)` for `Authorization: ApiKey …`, in this answer only (roll secrets' managed links use it instead of sharing the password); on an OpenSearch-flavor cluster (whose clients sign in with a user) the key is an internal user of its own: `{id, name, access, username, password, url, expires_at?}` without `api_key`, `username` being `<user>@<cluster>`, refused once `expires_in_s` passes |
| DELETE | `/v1/search/clusters/{id}/keys/{key}` | revoke it (204; 404 when it's gone). On a deleted cluster (by its id) the revoke is kept and applied by its restore (204) |
| GET | `/v1/search/clusters/{id}/browser-keys` | `{keys}`: the browser keys that still work, newest first, each `{id, name, indices or application, filter?, rate_limit, created_at, expires_at}` (no secret). Wakes the cluster |
| POST | `/v1/search/clusters/{id}/browser-keys` | a key a web page ships (above) `{name, indices or application, filter?, expires_in_s?, rate_limit?: {per_minute?, per_ip_per_minute?}, limits?: {max_concurrent?, max_result_window?, max_buckets?, max_items?, max_inference?, max_rerank_window?, max_body_bytes?, inference_endpoints?, allow_scripts?, allow_expensive_queries?}}` → the same fields plus `api_key` (once) and `url`. 400 on an OpenSearch-flavor cluster; 409 past 20 browser keys |
| DELETE | `/v1/search/clusters/{id}/browser-keys/{key}` | revoke a browser key (204; 404 for any other key of the cluster; on a deleted cluster, a browser key of it is kept for the restore, which revokes it: 202) |
| GET | `/v1/search/clusters/{id}/remotes` | the remote clusters it holds credentials for: `{remotes: [{alias, cluster, cluster_id, key_id, minted, created_at, connected}]}` (never the key) |
| PUT | `/v1/search/clusters/{id}/remotes/{alias}` | link another cluster of the same owner as remote `alias` (above): `{cluster, skip_unavailable?, access?}` or `{cluster, skip_unavailable?, api_key}` → `{alias, cluster, cluster_id, mode, proxy_address, key_id, minted, skip_unavailable?}`; 404 for a cluster the caller can't link to, missing or not. `admin` on both clusters for roll-access |
| DELETE | `/v1/search/clusters/{id}/remotes/{alias}` | unlink: the remote's settings and credentials go, a key roll made is revoked (204) |
| PUT | `/v1/search/clusters/{id}/repositories/{name}` | `PUT _snapshot/{name}` (Elasticsearch's body and query: `verify`), the only way a repository's credential settings can be roll secrets references (`roll://ENV/KEY`): the caller must be able to reveal each (403 if not; 400 for a cluster no project stands for or a key that doesn't exist), and is kept as their maker (architecture.md "Snapshots") → the cluster's answer |
| POST | `/v1/search/clusters/{id}/migrate/inspect` | list another Elasticsearch or OpenSearch's indices for a migration ("Migrating into roll" above) |
| POST | `/v1/search/clusters/{id}/migrate` | copy picked indices' definitions and start their reindex tasks (above) |
| GET, PUT, DELETE | `/v1/search/clusters/{id}/indices/{index}` | Elasticsearch's `GET`/`PUT`/`DELETE /<index>`: mappings, settings and aliases; create (body: `mappings`, `settings`); delete |
| GET, PUT | `/v1/search/clusters/{id}/indices/{index}/mapping` | `GET`/`PUT /<index>/_mapping` |
| GET | `/v1/search/clusters/{id}/indices/{index}/settings` | `GET /<index>/_settings` |
| POST | `/v1/search/clusters/{id}/indices/{index}/refresh` | `POST /<index>/_refresh` |
| POST | `/v1/search/clusters/{id}/indices/{index}/analyze` | `POST /<index>/_analyze` (a read) |
| POST | `/v1/search/clusters/{id}/indices/{index}/search` | `POST /<index>/_search` (reads take index expressions: `logs-*`, `a,b`) |
| POST | `/v1/search/clusters/{id}/indices/{index}/docs` | `POST /<index>/_doc`: index a document under a new id |
| GET, PUT, DELETE | `/v1/search/clusters/{id}/indices/{index}/docs/{doc}` | `/<index>/_doc/<id>` |
| POST | `/v1/search/clusters/{id}/indices/{index}/bulk` | `POST /<index>/_bulk` (NDJSON) |
| POST | `/v1/search/clusters/{id}/indices/{index}/delete_by_query` | `POST /<index>/_delete_by_query` |

The per-index routes are one Elasticsearch operation each, so the cluster, the index and the kind
of access (read, write, delete) are in the route: roll's scoped tokens and MCP tools map onto
them. A route's request runs as the cluster's owner narrowed to the index names in its path
(`x-roll-index-scope`: like an API key of the owner whose one role grants every privilege on
those names), so it reaches no other index: a bulk line's `_index` or an `mget` doc naming
another index is refused (403), and so is a lookup inside a query (a `terms` lookup, an indexed
`geo_shape`, `percolate`'s stored document, `more_like_this` documents, a `lookup` runtime
field): those are read as the caller, as Elasticsearch authorizes them. A document an ingest
pipeline moves (`_index`, `reroute`) is authorized again where it lands. ES|QL and SQL read as their client:
their field resolution and searches carry its credentials (wildcards expand to the indices it
may read, a lookup join's index is checked like `FROM`'s), and where its roles restrict
documents or fields the direct shard reads give way to searches, which apply them. Field
level security covers every read path a field's content or name can come out of: `_source`
(hits, gets, inner hits), `fields`, scripts' `params._source` and `doc` (script fields, runtime
fields, sort scripts), term vectors, field caps and mappings (`control.rs`
`field_security_hides_a_field_on_every_read_path`, 22 requests and 3 listings). They answer with Elasticsearch's own status and body, and pass the query string through
(`refresh`, `routing`, `size`, …). A write names exactly one index; a name starting with `_` is
refused (those are Elasticsearch's own APIs: use the console). Bodies may be up to the cluster's
`max_body_mb`.

To run the YAML REST suites against clusters made through the API, start rolld with
`--search-test-features` and point the runner at the create endpoint:
`search-yaml run --cluster-hook http://127.0.0.1:7070/v1/search/clusters --jobs 8 …` (each worker
gets its own cluster, deleted afterwards).

A new cluster's Overview (and the Indices tab's **Sample data**, any time) loads one of two datasets
roll generates in the browser from a seeded PRNG (`lib/search-samples.ts`: 1,000 products in
`sample-products`, 2,000 ECS web log lines over the last 24 hours in the data stream `sample-logs`)
through the console's `_bulk` in pages under the body limit, then runs three ready queries on it in
place (search as you type, facets, a filtered sort; errors in the last hour, top paths, ES|QL on
Elasticsearch-flavor clusters), each with Open in Console and copy as curl, JavaScript or Python.
Remove deletes the index, or the data stream and its template.

The dashboard (`/dashboard/search/<cluster>`) lists indices; selecting one opens its documents
(paged `_search` with a query string, as rows or as a table: the most common fields by default,
a column picker over the page's paths and the mapping's leaf fields, kept per index in the
browser, and click-to-sort on sortable columns, `.keyword` sub-fields for text; a time-based
index adds a time range and documents over time, newest first), mapping (a
field table: types, inference endpoints, vector settings, analyzers; or the JSON) and settings,
with refresh and delete. Import fills an index from a file (NDJSON, a JSON array, or CSV whose
columns are typed from the first 1,000 rows), previewed first and sent in `_bulk` batches through
the index's own route (`lib/search-import.ts`). Documents can be added, edited and deleted in place
(`refresh=wait_for`, routing kept), and any view opens in the Dev Tools console. The console
completes as you type (Ctrl+Space to ask): methods, paths and index names on the first line,
then the API's URL query parameters after `?` and their allowed values after `=` (from
Elasticsearch's REST API spec, `scripts/gen-search-params.mjs`);
body keys, query and aggregation types, mapping parameters, field types and the index's field
names (from its mapping) in the body, line by line for `_bulk` and `_msearch`. It highlights JSON
and copies a request as curl; a profiled search opens on its profile tree. The dashboard never re-encodes JSON it sends or shows: console and
document bodies go out as typed (`1.0` stays a float to dynamic mapping, longs above 2^53 stay
exact, key order is kept), `_source` is cut from the response text, and responses are re-indented
token by token (`lib/search-docs.ts`: `formatJson`, `hitSources`, `sourceCells`). Index names and mappings are fetched only when a completion needs
them, so opening the console never wakes a sleeping cluster. The Relevance tab edits synonym sets
(`_synonyms`: rules as rows or as text, ids kept, each save saying which indices' analyzers it
reloaded, and the index settings that use a set) and query rules (`_query_rules`: each rule as a
sentence, criteria and pin or hide actions edited in a form, reordered by `priority`, and a test that
runs the `rule` retriever and marks the pinned hits; `lib/search-relevance.ts`). Settings has the cluster's network access (its IP allowlist, one network a line). The dashboard reaches `/v1/search`
through rolld's API like every product (`VITE_ROLL_API`, the session from sign-in), and search
clusters appear in workspace projects (the `search` resource kind). A person who isn't an admin
sees their own clusters, and the Search page leaves out the storage location and the cache, which
`GET /v1/search` doesn't give them.

The Playground tab compares two configurations of one query on an index or data stream, side by
side (`lib/search-playground.ts`): keyword (`multi_match` over the mapping's text fields), semantic
(`semantic` on a `semantic_text` field, or `knn` on a `dense_vector` field with the query embedded
first through a text-embedding endpoint, because roll's `knn` doesn't resolve
`query_vector_builder.text_embedding`), hybrid (an `rrf` or weighted `linear` retriever over both),
or a raw body with `{{query}}`. Each hit shows its rank, score, `_id`, highlighted passages (the
`semantic` highlighter's chunks for `semantic_text`), how it moved between A and B, and an Explain
tree (`_explain`; a `semantic` query, which `_explain` can't rewrite, is explained by a `_search`
limited to that id; a hybrid hit shows each part). Either side copies as curl, Python or JavaScript
with the key from `$ELASTICSEARCH_API_KEY`, as the guides export it (OpenSearch-flavor clusters: OpenSearch's clients and
`$OPENSEARCH_USERNAME`/`$OPENSEARCH_PASSWORD`).

## Audit log

Every cluster has a security audit log, as Elastic Cloud's audit logging and OpenSearch Security's
audit log give one (`search-es` `xpack::security::audit` decides and writes entries,
`search-server` `audit` keeps them). The owner and admins read it from the API (`GET …/audit`)
and in the dashboard (Settings → Audit log: a table filtered by time and event, newest first).

- **What it records by default:** failed logins, access denials, `run_as` denials and security
  configuration changes: users (put, delete, password, enable, disable), roles, role mappings,
  API keys (create, grant, update, invalidate, also through `…/keys`), application privileges,
  service tokens, and OpenSearch Security's configuration documents. Granted access and
  successful logins only when the cluster asks: one entry per request.
- **Elasticsearch-flavor clusters** use Elasticsearch's event names (`authentication_failed`,
  `anonymous_access_denied`, `access_denied`, `run_as_denied`, `security_config_change`,
  `access_granted`, `authentication_success`) and entry fields (`event.type`, `event.action`,
  `user.name`, `user.realm`, `user.roles`, `user.run_as.name`, `apikey.id`, `origin.type`,
  `origin.address`, `url.path`, `request.method`, `request.id`, `action`, `request.name`,
  `indices`; a change's `put`/`delete`/`change`/`create`/`invalidate` object), checked against
  Elasticsearch 9.5.5. `xpack.security.audit.enabled` and
  `xpack.security.audit.logfile.events.include`/`exclude` are dynamic cluster settings, as on
  9.5.5 (event names checked: `invalid event name specified [x]`). roll's defaults differ:
  enabled, and the include list without `access_granted` and `run_as_granted`.
- **OpenSearch-flavor clusters** use OpenSearch Security's categories (`FAILED_LOGIN`,
  `MISSING_PRIVILEGES`, `COMPLIANCE_INTERNAL_CONFIG_WRITE` and `_READ`, `INDEX_EVENT`,
  `GRANTED_PRIVILEGES`, `AUTHENTICATED`) and `audit_*` fields (format version 4), and the stored
  `_plugins/_security/api/audit` config decides: `enabled`, `enable_rest`, `enable_transport`, the
  disabled categories (by default `AUTHENTICATED` and `GRANTED_PRIVILEGES`), `ignore_users`,
  `ignore_requests`, and compliance's `internal_config` with its ignored users.
- **Never in an entry:** a password, API key, token, `Authorization` or other header, query
  string or request body. A failed login names the user only when Basic credentials gave one.
  Changes are built field by field (a user's `has_password`, a grant's `has_password`), never by
  copying the body. OpenSearch's `log_request_body`, `write_log_diffs` and headers are accepted
  and never acted on. `control.rs` `audit_entries_hold_no_secrets` looks for every credential and
  marker of its requests in the answered and the stored entries.
- **Storage:** object storage next to the cluster's data
  (`search/c/<cluster>/audit/<first ms>-<last ms>-<token>.ndjson.zst`). A node batches a
  cluster's entries in memory for 5 s and writes them as one object, so a burst costs one write
  and a cluster nothing happens to costs nothing: no object, no timer (the flusher runs only while
  entries wait), and a read never wakes the cluster. The catalog was the other choice; it would
  put every failed login of every tenant into the shared Postgres of a fleet, while object
  storage is where a cluster's data already lives, scales with the cluster, and goes with it.
  Entries past `--search-audit-retention-days` are left out of reads and their objects deleted
  when the cluster's log is next written (at most every 10 minutes), as are the oldest past
  `--search-audit-max-mb`. Keys carry their time range, so a read fetches only the objects that
  overlap it, newest first, and stops once older ones can't make the page. A cluster's owner node
  records and writes its entries (requests run there; keys are unique per write, so nodes never
  overwrite each other); the owner flushes what waits before a read. A deleted cluster's entries
  go with its storage: kept with it for the deletion window and back with a restore
  (`Store::move_audit`), gone with its purge; a new cluster of the same name starts with none,
  and so does a branch. A node
  without object storage keeps them in memory within the same window and size.
- **Who reached it:** `origin.address` (`audit_request_remote_address`) is the client address the
  cluster's IP allowlist checks (architecture.md, "Client addresses and IP allowlists"): the
  connection's peer, the PROXY header's source, a trusted proxy's header, and on a fleet the
  address the node a client reached decided (`cluster::origin_of`), without a port. The node sets
  it and drops any copy a client sends; roll's own control-plane requests (the console, the
  per-index routes, `…/keys`) carry none.
- **Refused by the IP allowlist:** not recorded. The allowlist turns a request away before its
  credentials are read (and without waking a sleeping cluster), as Elastic Cloud's traffic
  filters turn it away at the proxy, where Elasticsearch's audit log never sees it.
  Elasticsearch's own IP filter (`xpack.security.http.filter.*`) would log `connection_denied`;
  roll's allowlist is the traffic filter, and a refused address costs a cluster nothing.
- **A sleeping cluster:** the node refuses a wrong cluster password without waking it, and records
  the failure as the cluster's filter said when it last unloaded on that node (its defaults if it
  never did there).

## Search analytics

Like Algolia's and Elastic App Search's analytics: per index, the queries people search for, the
ones that find nothing, and how often their results are clicked, so the Relevance tab's synonyms
and query rules can be aimed at real searches. The Relevance tab shows them first ("Search
analytics": totals, searches per hour or day, top queries, no-result queries and low
click-through queries), and each query offers "Synonym" and "Rule", which open the synonym and
query-rule editors prefilled with it (`query, ` as a rule; an `exact` `query_string` criterion).

- **Turning it on.** It's off for every index until the owner turns it on with the index setting
  `index.roll.analytics.enabled: true` (from the Relevance tab, or `PUT /<index>/_settings`). An
  index setting rather than a cluster setting: query text is the tenant's users' data, and an
  owner usually wants it for the one index behind a search box and not for logs or vectors;
  settings already carry per-index switches, follow aliases and data streams' backing indices, and
  are dropped with the index. roll registers it in both dialects with no default, so
  `include_defaults` stays Elasticsearch's.
- **What a search records.** Its text, from the `X-Roll-Search-Query` header when the client sends
  one, else the `q` parameter, else the user's text in the body: the first full-text query
  (`query_string`, `simple_query_string`, `multi_match`, `match` and its phrase forms,
  `combined_fields`, `semantic`, `sparse_vector`, `knn`'s `query_vector_builder` text, OpenSearch's
  `neural`) found through compound queries' scoring clauses (never a filter), then a retriever's
  (`standard`, `rrf`/`linear` and their simplified `query`, `text_similarity_reranker`'s
  `inference_text`, `rule`), then the `knn` section's. The text is lower-cased, its whitespace
  collapsed and cut at 128 characters. With it: whether the search found nothing, its hit count and
  its latency. `_search`, `_msearch` (each item) and search templates record; a search with no
  text (a `term` filter, `match_all`) records nothing.
- **Clicks** are Elasticsearch's own behavioral analytics events:
  `POST _application/analytics/<collection>/event/search_click` with the clicked `document`
  (`index`, `id`) and the `search` it came from (`query`, and `results.items` and `page` when the
  client has them, which give the click's position). The event is still indexed into the
  collection's data stream as Elasticsearch does; a click on an index with analytics on also counts
  toward its query's click-through and average position.
- **Aggregated in place.** Each index keeps one bucket per hour: exact totals and two bounded
  heavy-hitter lists (Space-Saving: 256 queries and 128 no-result queries per hour on the node; a
  query searched more than `searches / 256` times in the hour is always kept). The 60 s metering
  pass hands buckets to the catalog's fixed rings, as metrics are: 168 hourly buckets (7 days,
  100 queries each) and 31 daily ones (30 days, 300 queries each), a row per slot replaced when its
  time comes round. Nothing is ever a log of searches. A cluster holds at most 4 MiB of query text
  between passes; past it, new queries count only in the totals.
- **Cost.** While no index of a cluster has analytics on, a search pays one relaxed atomic load.
  With them on, recording is a mutex and a hash-map update after the search. Measured on a release
  build (`analytics::tests::record_cost` in search-es, 2026-11-18): the recording path alone (the
  text, the clock, the indices, the record) takes 630 ns, 0.47% of a `term` search for 10 hits on a
  10,000-document index through the REST handler (134 µs median); end to end, 12 alternating rounds
  of 5,000 such searches with analytics off and on differ by less than the rounds' own spread
  (medians 134 and 128 µs, rounds 114–141 µs). A sleeping cluster holds nothing in memory, runs no
  timer and writes no rows.
- **Reading it**: `GET /v1/search/clusters/{id}/analytics?index=&range=24h|7d|30d` (all indices
  without `index`). A read for roll-access; a person sees only their own clusters' (404 otherwise).
  The answer adds what the cluster's owner recorded since the last pass.

**Privacy.** No `Authorization` header, credential, API key or document body is ever recorded:
the only header read is `X-Roll-Search-Query`, and only a query's text, counted, is kept. Query
text is kept only aggregated as above, only while the index has analytics on, and for at most 30
days. Turning it off (or deleting the index, or replacing it with another of the same name)
deletes everything kept for it: reads stop showing it at once, the node drops what it holds, and
the next metering pass deletes the catalog's rows (a node that unloaded the cluster before that
hands the deletion on; one that died is overruled by the next load, which hands on the list of
indices with analytics on and the catalog drops every other index's rows). Deleting a cluster
deletes its analytics with its other rows when it's purged.

## Query suggestions

Autocomplete built from real searches, like Algolia's Query Suggestions and Elastic App Search's
query suggestions: roll turns an index's search analytics into a suggestions index and keeps it
fresh, so a search box's autocomplete needs no suggestion index built and refreshed by hand. The
Relevance tab's "Autocomplete" section turns it on, shows the heaviest suggestions and the last
build, blocks a term in one click and tries a prefix.

- **Turning it on.** Index settings next to `index.roll.analytics.enabled`, so they follow
  templates, aliases and data streams like it does (registered in both dialects with no default,
  so `include_defaults` stays Elasticsearch's). Analytics must be on too: suggestions come only
  from that index's own analytics.

  | setting | default | |
  | --- | --- | --- |
  | `index.roll.suggestions.enabled` | off | |
  | `index.roll.suggestions.min_count` | 5 | searches a query needs in the window |
  | `index.roll.suggestions.min_hits` | 1 | results its searches found on average (0 lets queries that found nothing through) |
  | `index.roll.suggestions.lookback` | `30d` | the window, 1h to the 30 days analytics keep |
  | `index.roll.suggestions.interval` | `1d` | how often it's rebuilt, at least `1h` |
  | `index.roll.suggestions.max` | 1000 | suggestions kept, at most 10,000 |
  | `index.roll.suggestions.block` | | queries never suggested (normalized as analytics keep them) |
  | `index.roll.suggestions.block_prefix` | | prefixes of queries never suggested |
  | `index.roll.suggestions.click_boost` | 1 | how much click-through raises a weight (0 to 100) |

- **Weight**: a query's searches in the window, raised by its click-through once clients report
  clicks (`search_click` events, as for analytics): `searches × (1 + click_boost × ctr)`, ctr at
  most 1. So a query people search less but pick a result for can rank above one they search more
  and abandon. Ties go to the more searched query, then by text
  (`search_es::query_suggestions::plan`).
- **A build** (`crates/search-control/src/suggestions.rs`), on the cluster's owner node: the
  index's analytics over the window (the catalog's hourly ring for windows up to 7 days, the
  daily ring beyond, plus what the node recorded since the last metering pass), at most 20,000
  queries, charged to the cluster's `request` breaker. The suggestions go into a new index
  `<alias>-<ms>` (one shard; a `completion` field `suggest` and a `search_as_you_type` field
  `query`, plus `weight`, `searches` and `clicks`; `_id` is the query). It's refreshed, then one
  `_aliases` call adds the alias to it and removes the previous build, so a reader sees the old
  suggestions or the new, never half of them or none. A build that fails partway deletes what it
  wrote; the alias never moved. When the suggestions come out exactly as the last build's (a
  fingerprint of each suggestion and weight), nothing is written. Builds of one index run one at
  a time.
- **Only its own names.** A build runs on the owner as the system, so what it may touch is fixed
  by construction, never by a setting: the alias is always `<index>-suggestions` (not
  configurable, so an index's settings can't aim a rebuild at another name), and each build is
  marked at creation with `index.roll.suggestions.source` and `source_uuid` (the source's name
  and uuid). They're private settings, as Elasticsearch's downsample marks are: a user's create
  or settings update that names them is refused, so only the build (which creates its index and
  the mark in one cluster state update, in process) can mark an index, and no one can mark an
  index of theirs, or one that already exists, as a build. A
  build refuses, records the error and writes nothing when the alias name is already a concrete
  index, a data stream or a data stream's alias, or an alias over any index without this source's
  mark. Moving the alias removes only indices carrying the mark of this very source index, and
  so does the privacy cleanup below.
- **When.** The catalog (`search_suggestions`) keeps, per index, the settings it was last built
  with, when it's next due, the last build (when, how many, how long, any error) and its 50
  heaviest suggestions. The metering pass builds what's due on the clusters loaded on its node,
  and at once when the settings changed (turning it on, a new block). A sleeping cluster isn't
  loaded to find out: every `job_wake_interval` its due suggestions are worked out from the
  catalog alone, and the cluster is woken (and billed as loaded) only when the build would change
  something; otherwise it's due again an interval on. An idle cluster with nothing new pays
  nothing. A cluster unloading records the settings it leaves with. A branch keeps its parent's
  suggestions index until it is due, then builds from its own analytics.
- **Serving.** Clients query the alias with the APIs they already use: the completion suggester
  (`"suggest": {"s": {"prefix": "sho", "completion": {"field": "suggest"}}}`, weights as
  `_score`), or a `bool_prefix` `multi_match` on `query`, `query._2gram` and `query._3gram`
  (Search UI's and InstantSearch's autocomplete through Searchkit). A browser key (#358) needs
  only `read` on the alias (`<index>-suggestions`), never on the source index.
- **roll API.** `GET /v1/search/clusters/{id}/indices/{index}/suggestions` (a read: settings, last
  build, next due, heaviest suggestions; never wakes the cluster) and `POST
  …/suggestions/rebuild` (a write: builds now and answers with `changed`). Both are about
  `{index}` for roll-access and are MCP tools (`search_suggestions`,
  `search_rebuild_suggestions`).

**Privacy.** Query text is the tenant's users' data. Suggestions only ever come from that index's
own analytics, and a query searched fewer than `min_count` times in the window (default 5) never
becomes a suggestion, so one person's unusual search isn't shown to everyone else; a blocked one
never does either. The suggestions index and the catalog's row hold only what passed. Turning
suggestions off stops rebuilds and leaves the index for the owner to delete. Turning analytics off
(or deleting or replacing the source index) deletes the suggestions too, as it deletes the
analytics: the catalog's row goes with the analytics' rows, and the next metering pass on the
cluster's owner deletes every suggestions index roll built from it (marked at creation with
`index.roll.suggestions.source` and `source_uuid`).

## Metering and pricing

Every 60 s the control plane accounts each loaded cluster's loaded time exactly (load and unload
come from the node's lifecycle hooks; an unload accounts the remainder, rounded up to a second),
its requests (every request the node admitted for it: clients', the console's and the per-index
routes') and its resident size, measured from the engine: the heap its shards hold (segments still in
memory, readers' heap and caches, indexing buffers, the version map) plus 16 MiB for the cluster
itself. Size is measured at every tick and as the cluster unloads, and each interval is billed at
the larger of the measurements at its two ends. While a cluster is loaded with every index open,
its document count is the engine's (unflushed writes included); otherwise it comes from the
commit points. Both count Lucene documents (nested ones included), as `_stats` does, so the figure
doesn't jump when the cluster unloads. Stored bytes are re-read from the cluster's commit points after every load and
unload and every 10 minutes while loaded; storage accrues from a change log, so a sleeping
cluster adds no rows. A deleted cluster kept for restoring keeps its last size in the log, so its
storage is billed until it is purged, when its rows go. Inference on the node's embedding pool
(the E5 endpoint, `semantic_text` with it) is billed by the CPU it took: each job's thread CPU
time is measured on its worker and handed to the meter with the requests
(`inference_cpu_seconds`, priced per vCPU-hour as `usd_per_vcpu_hour`), to the cluster whose
request runs it: a linked cluster's cross-cluster query that embeds on this cluster, or a
browser key's search, is billed here (the browser keys of #358 will cap inference per request). Prices are AWS list prices in `crates/search-control/src/pricing.rs`, each
with its source.

## Tests

```sh
pnpm test:search              # scripts/test-search.sh: skips itself when nothing changed
scripts/test-search.sh --unit # no Docker
```

`scripts/test-search.sh` builds and lints every `search-*` crate, `rolld` and `roll-cli`, runs
their tests (the control plane's integration tests in `crates/search-control/tests` drive the
API, the node and an Elasticsearch client over HTTP, and `cluster.rs` two nodes of a fleet;
`crates/roll-cli/tests/search.rs` runs the real `roll` binary against an in-process control
plane), the dashboard's console completion and document helpers
(`apps/dashboard/src/lib/search-*.test.ts`, vitest), then the YAML REST suites against
`search-dev` for both dialects. A suite passes when its pass count is at least the minimum in
[compatibility.md](compatibility.md), written as

```
<!-- test-search: roll >= 1234 -->
<!-- test-search: roll-os >= 567 -->
```

Raise the minimum whenever the count goes up, so it can't silently go back down. The
differential suite against a real Elasticsearch (`crates/search-server/tests/differential.rs`)
and the client matrix (`tests/search/clients/run.sh`) run when present. CI:
`.github/workflows/search.yml`, triggered from `ci.yml` when search code changes.

## Status

Measured on the tree merged with `main` (2026-11-12), on ai-server (a shared 18-core host):

| check | result |
| --- | --- |
| build, `cargo clippy --all-targets -D warnings` (search crates, rolld, roll-cli, roll-workspace, roll-edge) | clean |
| `cargo test`, every search crate + rolld + roll-cli | 1,949 passed, 8 failed, 46 ignored (134 binaries) on the merged tree; every failure fixed since (below) |
| Elasticsearch 9.5.5 YAML REST suite (`test/free` + clients-tests) | **3,810 of 3,905 pass**, 0 fail, 95 skipped; 0 regressions against the baseline |
| OpenSearch 3.9.0 YAML REST suite | **1,455 of 1,506 pass**, 0 fail, 51 skipped |
| OpenSearch SQL plugin's PPL suite | **204 of 204** |
| extended suites (67 module, plugin and x-pack suites, each on its test cluster's settings) | **5,083 of the 5,541 tests that pass on Elasticsearch** (6,349 in all); 46 suites at 100% (EQL 39 of 39, ent-search 371 of 371, ES\|QL 549 of 549, KQL 34 of 34, `otel-data` 68 of 68, SLM 8 of 8, searchable snapshots 13 of 13, transforms 108 of 108, watcher 89 of 89, `watcher-security` 9 of 9), runtime fields 248 of 248, `logsdb` 233 of 234; round 6 passed 3,463. Largest gaps: x-pack `platinum` (1,064 of 1,412: rollup 52 of 52, security 100 of 101), downsampling (58 of 59) |
| differential suite against Elasticsearch 9.5.5 (seed 1, 60 random workloads) | 2,325 of 2,333 steps equal (99.7%); the 8 others are 2 known differences (soft-delete tombstones in phrase-suggester and term statistics), none new or open; 69 replayed fixtures equal |
| client matrix (unmodified official clients in Docker) | **11 of 11**: Python, JavaScript, Java, Go, Ruby and PHP clients, the OpenSearch Python and JavaScript clients, django-elasticsearch-dsl, Laravel Scout, and searchkick's own suite (540 runs, 0 failures) |
| x-pack REST fixtures (`search-es/tests/xpack_fixtures.rs`) | 1,097 of 1,097 steps equal Elasticsearch |
| inference API (`_inference`, against a private Elasticsearch 9.5.5 and a mock of the services) | OpenAI flows, 42 error and update cases and region policies answer the same; `platinum/inference` 22 of 23 (was 1); `inference` suite (the test plugin's services, no Elasticsearch baseline) 161 of 305; `semantic_text` through a mock as a writer with only index privileges |

The failures on the merged tree, all fixed: rolld tests that collided on search's default port;
two tests that depended on timing; three behaviours the uncommitted round-6 work had broken (SQL
`CAST … AS VARCHAR`, ES|QL keys under a field `flattened` in another index, connector key order
after an update, checked against the reference Elasticsearch); a by-query task test that read a
filtered listing; and the auth matrix's probe of storage's nested route (also broken on `main`).

## Next

Ranked; the top item is in progress. Updated as work lands (the watchdog reads this list).

Landed 2026-11-17: per-person cluster caps (#162: 50 clusters and 2 always-on per person,
rolld flags, 409 past a cap, counted in the write's own transaction); the search playground (#159); the Relevance tab for
synonym sets and query rules (#170, with an updateable synonym analyzer refused at index time as
Elasticsearch refuses it); knn CPU on merged segments (#180); the parked-governor test hang
(#181); runtime fields' and logsdb's last gaps (#187: 248 of 248 and 233 of 234); a branch made by
create with a `parent` needs `write` on the parent (#188).

Landed 2026-11-18: deleted clusters recoverable for 7 days (#172: restore, `?deleted=true`, a purge sweep on one node, held to the caps; an unlink while deleted is recorded and applied at restore); by-query scans run off the reactor, so one tenant's long script can't stop a node's I/O (#211); the `s3` repository's multipart uploads, `chunk_size`, throttling (0.89-0.98 of each setting, never over) and a real `_analyze` (#237); cluster performance metrics and the search and indexing slow logs, with a Performance tab (#241; about 0.5% CPU on a cheap search, nothing for a sleeping cluster); people no longer see which node holds their cluster; IP allowlists per cluster (#243: checked before credentials on the address a trusted front gives, carried signed on a fleet; `/internal/search/rest` takes only signed requests).

Landed 2026-11-18 (evening): a security audit log per cluster (#247: failed logins, denials
and security changes in Elasticsearch's or OpenSearch Security's own format, never a secret or a
body); search analytics (#291: per index, opt in with `index.roll.analytics.enabled`; top
queries, no-result queries and click-through on the Relevance tab); the rest of
Elasticsearch 9's inference services (#265, matched against Elasticsearch's own upstream
requests); `gcs` and `azure` snapshot repositories (#293); the rest of ILM and the data stream
lifecycle (#297); timing tests that load can't break (#312); and the request path's large
futures boxed (#321: a debug build's deepest stack in the cluster test fell to 836,912 B of
a worker's 2 MiB, so the six above could land together).

Landed 2026-11-19: Terraform for search clusters (#356: `roll_search_cluster` with a flavor
switch in place, `roll_search_api_key` and its ephemeral form, `roll_search_snapshot_repository`,
the cluster data source); cross-cluster search between a person's own clusters (#347); the
dashboard's Snapshot and Restore (#325) and Ingest Pipelines (#339); inference keys from a
project's roll secret, kept in step on rotation (#134); the inference `timeout` capped and file
service tokens limited to the clusters an operator names (#332); Rally Round 11, noaa and
nyc_taxis with Elasticsearch's merges flowing (#340).

Landed 2026-11-20: a sweep for the bug classes the 2026-11-19 reviews kept finding (#434:
depth limits in every query language, so one crafted ES|QL, SQL, EQL, PPL, KQL,
query_string or PromQL query can't abort a shared node; `_explain` under document and field
level security; restricted readers bypass the shard request cache); cross-cluster search's
gaps (#389: every aggregation from partial state, scroll and point in time across clusters,
the Remote clusters panel; scrolls and points in time bound to their opener); query
suggestions from search analytics (#382); the search suite nightly on main, with an issue on
failure (#426); knn beams sized to each segment (#401, Rally's last vector loss now a tie);
nyc_taxis bulk indexing at -28% instructions per document (#417); and #347's expiry test made
deterministic (#396).

1. **Search from the browser** (landed in #358 after six reviews, its policy enforced in the
   engine on the parsed request: per-cluster CORS, browser keys with rate limits and their own body
   cap, a Browser search section and the site-search guide's browser page; what remains is RAG's
   chat limits for browser keys, item 11; Algolia's search-only and secured API keys,
   Typesense's scoped keys, Meilisearch's tenant tokens, Elastic's search applications): a web
   page can't call a roll cluster today. There's no CORS (`http.cors.*` are static settings no
   tenant can set), so every site needs a backend in front of search, which is the first thing
   those products remove. Per-cluster allowed origins (set from roll's API and the dashboard, as
   Elastic Cloud's user settings set `http.cors.*`), keys a page can ship (search-only, limited
   to indices or a search application, filters baked in, an expiry, a rate limit per key),
   Elasticsearch's `_application/search_application/{name}/_search` as the safe surface, and a
   quickstart that works with Elastic's Search UI and InstantSearch through Searchkit. An IP
   allowlist still applies to browsers; the docs say how the two combine.
2. **Migrating into roll** (landed in #485 after six reviews: reindex from remote over HTTPS
   through roll's guarded client (`roll_net::PublicOnly`, private literals and rolld's own
   addresses refused, no redirects, bounded reads and timeouts), public hosts allowed without a
   node allowlist (an operator's allowlist still narrows it), Basic and API key credentials sealed
   and never echoed, remote pages charged by what parsing builds; and the dashboard's migration
   wizard: `inspect` lists a remote cluster's indices (fetched by roll, its answer charged until
   it's sent), copies mappings, settings and aliases, reindexes with progress and switches an
   alias when done. Elastic Cloud restores from other deployments, AWS OpenSearch Service has
   migration assistants, Algolia and Typesense import from each other): left, Algolia's export
   (records, synonyms, rules) mapped onto indices, synonym sets and query rules, and a Typesense
   and Meilisearch import (their dump formats onto indices and settings).
3. **One memory account per request** (in progress: ES|QL and aggregations in #549, Painless per execution in #591, and a
   node-wide body budget in #601, each in review; ES|QL and aggregations against a node budget
   no tenant setting can raise in #549, Painless per execution, and a node-wide body budget; Elasticsearch charges every request's
   memory to its circuit breakers, ES|QL's blocks included, and Elastic Cloud's per-deployment
   limits hold because of it). roll serves many tenants per node, so a request whose memory the
   breakers don't see is a way for one tenant to stop everyone's searches. Today's reviews found
   three: remote reindex pages charged 3x their bytes while parsing builds about 80 bytes per value
   (#485), connector rows parsed into trees (#318), and ES|QL, which charges nothing (`exec.rs`
   collects rows and columns with no breaker: `STATS` groups, `SORT`, `MV_EXPAND`, `LOOKUP JOIN`
   and `ENRICH` tables, `VALUES` and `TOP` states). The fix for ES|QL is Elasticsearch's: every
   page of rows charged to the request breaker as it is built and released when dropped,
   `circuit_breaking_exception` past the limit, and a node-level limit no tenant setting can
   raise. Time has its own cap (#530, landed: one limit per request, by-query scans and internal pagers included). Rule: charge what a structure costs as it is
   built, never a multiple of the bytes it came from. Also left: `_bulk`'s own preparation (`bulk_ahead`
   parses each source into a tree) is charged only as the body's `inflight_requests`, for
   clients' bulks and connector syncs' alike (#318's pages are text until then).
4. **A sweep for the bug classes the 2026-11-19 reviews kept finding** (first pass landed in #434, the follow-up in #492; what
   today's reviews still find is resource accounting, item 3). Four
   independent security reviews (connectors #318, cross-cluster search #347 and its gaps #389,
   browser search #358) each found real bugs, in the same few shapes: recursive decoding with no
   depth limit (a stack overflow aborts a shared node), memory held past a request with no breaker
   accounting it, get-clone-put state that a concurrent close undoes, credentials kept when the
   host they go to changes, sealed values a client can plant, an alias wildcard that skips one
   authorization path, and secrets or keys a branch inherits from its parent. Only the code those
   reviews read has been checked. Every decoder, cache, context store, credential field and branch
   copy in search gets the same questions, with a test per real finding.
5. **Cross-cluster search: what's left** (ES|QL across clusters landed in #504 after five reviews: each
   cluster's part on its owner, `_remote`/`_coordinator`/`_any` enrich modes, LOOKUP JOIN across clusters
   with union types refused as Elasticsearch refuses them; still different: a command inside a part after
   an `ENRICH` reads a field the clusters type differently, where Elasticsearch refuses any use; landed in #347, after its security review's fixes: an alias wildcard matching a link and a
   loopback remote skipped the local index check, and merges were quadratic and unbounded;
   same owner only,
   checked at link time and on every request, `cross_cluster` keys issued on the remote and kept
   sealed, the remote's part run on its owner node, never over the public endpoint; `_clusters`,
   `skip_unavailable`, `_resolve/*`, `_field_caps` and `_msearch` as Elasticsearch 9.5.5 answers).
   Landed in #389: every aggregation merged from each remote's partial state
   with one final reduce, `scroll` and point in time across clusters, the dashboard's Remote
   clusters panel, OpenSearch 3.9's `cluster.remote.*` answers, ES|QL `FROM <alias>:<index>`
   (each cluster's part up to its first breaker, `STATS` merged from partial states,
   `ENRICH`'s remote modes, `_clusters`). Left: linking OpenSearch-flavor clusters (no `cross_cluster` keys there:
   it needs a credential model and a review of its own).
6. **The Rally losses left by the full rerun** (Round 10, vectors, #268: Elasticsearch 9.1+ runs no merge while less than ~93.5 GB of disk is free, and this host was 91-98% full, so Round 9's Elasticsearch races were held on vectors and noaa (4 of 6); the harness now sets `indices.merge.disk.check_interval=0`. With merges flowing, the force-merged vector losses are gone and dense_vector `100-1000_multiple_segments` (2.30x) was the one loss with ranges apart; Round 12 makes it a tie (0.97, 6.13 against 6.35 ms; 0.83 at equal foreign load): each segment now searches a beam sized to its share of the shard, as Lucene 10.5 does, −33% instructions per query, recall unchanged. Round 11, noaa and nyc_taxis rerun with merges flowing at `769dd6744` (#340): Round 9's noaa claims held (every range-field query ahead in median, all ties; `conjunction_big_range_big_term` 7.4 against 12.2 ms, faster in 6 of 6 rounds; indexing 0.75, ahead in 6 of 6), and nyc_taxis wasn't held at all (its 0-merge races are Elasticsearch's merge policy). What changed: nyc_taxis `distance_amount_agg` leads by less (46 against 78 ms, 0.59; Round 9's 118 was load), its `range` now leads (0.88), and Elasticsearch now indexes nyc_taxis faster in median (1.03 docs/s, bulk p50 1.04, ties)) (docs/benchmarks/search.md: Round 8, every Rally
   track against Elasticsearch 9.5.5 and OpenSearch 3.9 at `83fac5982`; Round 9, the four tracks
   #29 fixed rerun at `c2b47b251`, three alternating rounds each, noaa six; Round 11, noaa and
   nyc_taxis again at `769dd6744`; Round 13, nested and http_logs at `8a328a7d2`). Roll leads most tasks
   1.3-4x (percolator up to 300x) and indexes as many or more docs/s than Elasticsearch on every
   track but nyc_taxis (a tie, Elasticsearch 1.03x ahead in median), on less server CPU per race
   than Elasticsearch wherever it was measured (on noaa only in median: 347 against 360 s). Round 9
   confirmed #29's fixes under
   Rally: noaa `range_field_conjunction_big_range_big_term_query` 5.12x → 0.67 (6.8 against 10.2
   ms; Round 11 0.60, 7.4 against 12.2), noaa indexing 1.29x → 0.86 (tie; server CPU per race
   -23%; Round 11 0.75), nested inner hits `big_size`
   2.25x → 0.52 and `_default` 1.20x → 0.74, nyc_taxis `distance_amount_agg` 1.27x → 0.50 (59
   against 118 ms, still on one thread; Round 11 0.59, 46 against 78), `date_histogram_agg` →
   0.39 (Round 11 0.37), geonames
   `desc_sort_population` → 0.64 and `asc_sort_population` → 0.83 (ties); `autohisto_agg` is still
   a tie with OpenSearch (0.87; Round 11 0.93). Round 9's vector rerun (`807396e0b`, against Elasticsearch
   only): Round 8's so_vector `css` 1.92x and dense_vector `_multiple_segments` 2.09x are ties now
   (1.13x, 1.51x), so_vector `concurrency` and `random-10-percent` lead (0.81, 0.74, ties), and
   roll indexes dense_vector at 3x Elasticsearch's docs/s on less server CPU. Where roll still
   loses, ranked: so_vector's 8-client tasks on the force-merged index, `concurrency` 2.80x and
   `random-10-percent` 2.32x, then `concurrency-force-merge` 1.45x, dense_vector
   `knn-search-100-1000` (merged) 1.17x and `random-10-percent-force-merge` 1.11x (8 clients on 2
   cores: latency follows CPU per query). Request parsing rescanned the body for field locations
   no message used, 8-15M instructions per so_vector query; #156 computes them only for a
   message (-25 to -61% server CPU per query on 16 of 20 lane-b tasks, hits identical). On the
   force-merged index #180 searches a filter too small for the graph exactly and scores only the
   filter's docs: `concurrency` at 8 clients 2.83 → 1.05 ms server CPU per query, p50 6.43 → 2.75
   (Elasticsearch 6.09 in Round 9), `random-10-percent` -8%, `default-match-all` -15%, hits
   identical but where exact (1M corpus recall unchanged, 1%-filter 0.798 → 0.813). Rally rerun
   of so_vector and dense_vector pending. Measured, not adopted: Lucene's per-segment beam (-41% work, but recall on the
   1M corpus fell 0.9935 → 0.9895) and an exact path for small filters (fewer instructions, lost
   8-15% to code placement; kept for a retry with placement controlled); the pmc and so bulk p50s (Round 8, ties). Round 13 (nested and
   http_logs at `8a328a7d2`, 3 rounds against Elasticsearch, http_logs also against OpenSearch):
   #125 holds under Rally (nested `randomized-sorted-term-queries` 4.05 against Elasticsearch's
   15.02 ms, 0.27), and `nested-date-histo` (1.29, 293 against 227 ms: about 450 instructions per
   nested doc, 1.62G per query) is now counted from the nested field's points tree when every
   root matches (the shard query of a nested index, `match_all` filtered to the roots, wasn't
   recognised as matching all, so no points shortcut ever fired there): 21M instructions per
   query, 9.3 against 324.5 ms under Rally (0.03), identical results; the collected path is 21%
   cheaper too. Not losses: the nested bulk p50 (1.17-1.23x; Elasticsearch's latency is bimodal,
   p99 2.1-4.2 s against roll's 0.8-1.05, and roll's mean is lower at more docs/s), http_logs
   `scroll` (1.09x OpenSearch, ranges touching; roll spends 280 ms of server CPU per scroll
   against OpenSearch's 484-499), `sort-size-desc-after-force-merge-1-seg` (1.03x OpenSearch,
   tie; 0.73 Elasticsearch) and `200s-in-range` (1.04x OpenSearch, tie). Round 9's noaa `range_field_disjunction_big_range_small_term_query`
   1.06x and nyc_taxis `range` 1.05x are gone in Round 11 (0.93, a tie; 0.88). Rerun next: so_vector with #156, so, pmc and percolator at the current
   head (http_logs' indexing fix `6c2834a3e` holds under Rally in Round 13: 140k docs/s against
   Round 8's 130k and Elasticsearch's 96k). Not a loss: `terms_enum` (OpenSearch has no such API; its
   "0.57 ms" was an error). nyc_taxis indexing, Round 11's bulk p50 1.04x and docs/s 1.03x
   Elasticsearch (ties; slower in 3 of 3 rounds): 40% of the server's instructions were zstd level 6
   compressing `best_compression` stored fields, once when a buffer became a segment and again in
   every merge; buffer segments now use level 1 and merges keep level 6 (-28% instructions per
   document at 8 clients, -15% at 1; store +0.2%; random fetches unchanged; Rally: +19% docs/s and
   -8% server CPU per race against the parent the same hour, at equal foreign load;
   docs/benchmarks/search.md, "nyc_taxis bulk indexing"). Elasticsearch's own codec (level 3,
   240 KiB blocks, no dictionary) saved as much but made random fetches 3x dearer. Next there: fewer,
   larger buffer segments without more memory (roll's buffer holds 1.4 KB per nyc document against
   Lucene's under 290 B; 564 B of it stored fields Lucene writes as they arrive; a 256 MiB budget
   saved another 4% at 8 clients and 25% at one). Still open from before: one-client indexing CPU
   (51.8 against 34.6 µs/doc on the ECS log; its profile has BP reordering at merge time at 14% of
   instructions), BP only for time-series and log indices, the request path's allocations, one
   allocator policy.
7. **Connectors that sync** (landed in #318, eb364e2b3, after eight reviews: a runner on the
   cluster's owner node, scheduled like ILM so an idle cluster pays nothing between syncs, with
   `_connector/_sync_job` rows as Elasticsearch keeps them; `postgresql` full and incremental, `s3`,
   sync rules and a Connectors tab. A sync writes only its own index: a pipeline that moves a
   document elsewhere fails it, and its `_bulk` takes only `index` actions naming `_id`. Credentials
   are sealed to the cluster and refused on a node without `ROLL_SECRET_KEY`, which fields count as
   credentials comes from roll's schema, and branches keep inherited connectors paused until
   adopted with new credentials. Elastic's connectors, Algolia's and Typesense's data sync): left,
   linking a person's own roll Postgres or bucket with a read-only credential of its own,
   credentials from roll secrets after #134, Postgres advanced rules, PDF and Office text, and the
   web crawler (item 13) on this runner.
8. **Semantic search with no keys** (built in #563, in review: roll's float build of E5 equals
   Elasticsearch's float deployment, cosine 1.000000 on 64 of 64 cases; Elastic Cloud's `.multilingual-e5-small-elasticsearch`
   and Elastic Inference Service, Typesense's and Meilisearch's built-in embedding models,
   Algolia's NeuralSearch): every `semantic_text` field and `text_embedding` endpoint on roll needs
   the customer's own provider key today, so the first semantic search means an OpenAI or Cohere
   account. Run `multilingual-e5-small` (MIT, 384 dims, 100+ languages) in roll itself as
   Elasticsearch's preconfigured `.multilingual-e5-small-elasticsearch` endpoint answers, so
   `semantic_text` with that `inference_id` (and Elasticsearch clients and the YAML tests that use it)
   works unchanged with no key. In Rust on CPU (no Python, no GPU), on a node-wide pool with
   per-cluster fair shares and a cap, its CPU metered like indexing, the weights fetched once into
   the node's cache and checked by digest; an idle cluster still costs nothing. ELSER's weights
   are under Elastic's terms, so `.elser-2-elasticsearch` stays as Elasticsearch answers without
   ML nodes; an open sparse model and a reranker are follow-ups once E5 is measured.
9. **Snapshots on the fleet.** Landed: the `gcs` and `azure` repository types (#293); and every cluster whose data is in object storage (the fleet
   and a single rolld) has a default repository, `found-snapshots` as on Elastic Cloud
   (`--search-default-repository`), so `_snapshot` and snapshot lifecycle policies work without
   setup; it can't be changed or unregistered, costs an idle cluster nothing (registered in
   memory on open, no policy of its own) and keeps snapshots as pins in the cluster's own
   storage (branches copy them, they go with the cluster). And the `s3` repository type: a bucket
   the tenant names, reached through `roll_net::PublicOnly`, credentials sealed apart from the
   settings and never shown, snapshots uploaded as documents per segment (incremental), so
   another cluster restores them (docs/search/architecture.md, "Snapshots"). Left: `_analyze`
   against a real bucket (it plans and answers without touching one), multipart uploads and a
   `chunk_size` setting (a chunk is ~8 MiB; one object per chunk), the `gcs` and `azure` types,
   `max_snapshot_bytes_per_sec`/`max_restore_bytes_per_sec` throttling of bucket traffic, and
   credentials from roll secrets instead of the repository's settings (landed in #361: `roll://ENV/KEY`
   references, the maker checked again on every use). Restoring from a snapshot Elasticsearch wrote:
   in progress (read-only foreign repositories, `_source` read from the Lucene stored fields and
   reindexed). It means reading Elasticsearch's repository
   (`index-N`, `snap-*.dat`, `meta-*.dat`, shard `index-*`/`snap-*.dat`: SMILE inside Lucene
   codec headers, DEFLATE-compressed) and each shard's Lucene files (segment infos, field infos,
   compound files, LZ4/DEFLATE stored fields for `_id`, `_routing`, `_source`, numeric doc values
   for `_seq_no`, `_primary_term`, `_version`, live docs, flat vectors for excluded source
   vectors; Lucene 9 and 10 codecs for 8.x and 9.x snapshots), all as hostile input, then
   re-indexing every document from `_source`: about 4,000 lines and Elasticsearch-made fixtures,
   and a restore that is a full reindex. Indices without a stored `_source` (synthetic source,
   the logsdb and time-series default, `_source.enabled: false`) can't come back that way.
   Decided 2026-11-17: not now. A live cluster moves by reindex from remote (item 2: today it
   can't, over HTTPS); build it once tenants ask to move by backup alone.
10. **Relevance tuning by API, CLI and MCP** (in progress, with `roll search keys create` taking
   index patterns and read-only access; Algolia's API and CLI `rules`/`synonyms import`,
   Elastic's APIs): `/v1/search` has no routes for `_synonyms`, `_query_rules` (and `_test`), writing
   an index's settings, or `_explain` (`search-control/src/api.rs`), so roll's MCP has no tools for
   them, nor do `roll search` and the Terraform provider; an agent can create a cluster, index and
   query, then stops. Writing settings also unblocks the suggestions tools (they need
   `index.roll.suggestions.enabled`), and a redacted answer would let `POST …/inference/secrets`
   become a tool, so an agent can set up semantic search end to end.
11. **Conversational search and RAG** (phase 1 in progress: streamed `chat_completion` and
   `completion` inference and the Playground's Chat tab; Elastic Playground's chat with citations, Typesense
   `conversation=true`, Meilisearch `/chats`, OpenSearch's `retrieval_augmented_generation`
   processor): roll serves `completion` and `chat_completion` inference already, but the Playground
   compares retrieval only. Chat over hits with citations, the code for a RAG app, and a chat API
   (OpenSearch's processor shape on OpenSearch-flavor clusters). Design: [rag.md](rag.md).
12. **The first query in a minute** (the guides landed in #604; sample data and a guided first
   query landed (#658): one click loads 1,000 generated products or 2,000 ECS log lines on the
   Overview of a cluster with no indices or from the Indices tab, then three queries run in place
   with Open in Console, copy as curl, JavaScript or Python reading `$ELASTICSEARCH_API_KEY` as the
   guides do, and a link to the keys panel). Left: a key made inline in that panel (today it links
   to Settings → API keys); a sample dataset for RAG (passages with `semantic_text` on E5); and
   what writing the guides found: an API key's `encoded` value works only at the cluster's own
   host, so clients behind a shared address re-encode it as `<id>@<cluster>:<key>`; a `data` key
   can't post click events, so an app tracking clicks needs a second key; a `semantic_text` field
   with no `inference_id` fails, because its default (`.elser-2-elasticsearch`) answers as a
   cluster without ML nodes, the first error anyone copying Elasticsearch's docs hits; and no
   screen is named Discover.
13. **A web crawler for site search** (design in [crawler.md](crawler.md), ready to build now that
   #318's runner is on main; Elastic Open Crawler,
   Algolia Crawler, DocSearch, Meilisearch's docs-scraper): connectors (#318) sync Postgres and S3;
   getting a website in is the customer's job. A native connector roll runs (`roll_web_crawler`,
   on #318's runner) taking Open Crawler's config, fields and ids, fetching through
   `roll_net::PublicOnly` with redirects checked hop by hop, robots.txt always honoured, domain
   verification for more than small crawls, and a purge that a site outage can't empty.
14. **Semantic search on OpenSearch-flavor clusters** (AWS OpenSearch neural search):
   `neural`, `neural_sparse` and `agentic` are names only (`meta/aliases.rs`); ML Commons
   (`_plugins/_ml/connectors`, `models/_register`) and the `text_embedding`/`ml_inference` processors
   aren't served, so OpenSearch customers' neural setups (Bedrock or OpenAI connectors) fail on
   arrival. Map them onto roll's `_inference` services and E5 (#563).
15. **Relevance evaluation, A/B tests and merchandising** (Algolia A/B Testing, rules that
   boost, bury, filter and redirect, conversion analytics, dynamic re-ranking): `_rank_eval` is served
   without a UI or judgment lists, the Playground's A/B is offline, analytics records clicks only, and
   rules stop at pin and exclude. Live traffic splits through search applications (#358), conversion
   events, and rule actions beyond Elasticsearch's (designed first as roll extensions).
16. **Cold start at size**: the 19.8 ms cold start is a 50k-document index from local cache;
   nothing measures the first query and p99 on a node with an empty cache for 1, 10 and 50 GB
   indices loaded from object storage, or on Graviton. Buyers comparing always-on Typesense or Algolia
   will ask (benchmark lane).
17. **Private networking and customer keys** (Elastic Cloud PrivateLink traffic filters and BYOK,
   AWS OpenSearch VPC domains and KMS): the data plane is public with IP allowlists; roll apps and
   previews send from per-task public IPs on roll cloud, so an allowlisted cluster refuses them. A
   signed link identity an allowlist can admit ("apps service svc_x"), private endpoints, and a
   cluster key the customer holds (design first).
18. **Logs end to end** (Elastic Observability's managed OTLP intake, OpenSearch Ingestion and the
   Alerting plugin): no OTLP intake in search, `_plugins/_alerting` isn't served, Watcher has no UI,
   nothing alerts on a cluster's own p99, 5xx or 429s, and Discover has no saved searches or
   visualizations. An OTLP logs and traces intake into data streams also gives roll apps' log drains a
   home in roll search.
19. **Vector search's footprint** (docs/benchmarks/search.md, "Vector search": 1M × 384 against
   Elasticsearch 9.5.5 and OpenSearch 3.9). Roll leads on indexing (4,735 docs/s against 3,585
   and 2,978 at 8 clients), merged `knn` (1.9 ms CPU against 6.1 and 4.9), `rrf` (3.2 against 13.8
   and 8.3), cold start (36 ms to the first `knn` against a ~25-29 s node start) and memory
   (34 MiB idle against 2.5 and 4.1 GiB); filtered `knn` now reads its filter as a bitset (about
   half the CPU, identical hits). Store bytes are now Elasticsearch's (1.74 GB force-merged
   against 1.71, was 2.67): `_source` is stored without indexed vectors under
   `index.mapping.exclude_source_vectors` (Elasticsearch 9's default) and every reader patches
   them back as Elasticsearch prints them; fetching `_source` got cheaper too (vectors hidden 7.9
   → 2.0 ms CPU per `knn`, shown 13.5 → 8.7). Left in `_source`: vectors in `nested` objects and
   `sparse_vector` values. A `knn` now searches unmerged segments concurrently, one thread per
   physical core (16 segments: p50 5.5 / 10.8 / 13.9 → 4.3 / 7.0 / 11.7 ms unfiltered / 10% /
   1% filter, below Elasticsearch's 12.9 / 13.4 / 13.9, identical hits; CPU per query +3-25%),
   and `int8_hnsw` construction prefetches codes (676 → 608 µs CPU/doc, same graphs). Measured,
   not adopted (`wip/search-vectors2`): AVX-512 kernels (float bit-identical to AVX2, VNNI for
   codes) run 4-22% slower on this Cascade Lake, and prefetch doesn't help float or one-bit
   construction, so float HNSW indexing still costs 1,250 against 1,174 µs/doc (60% `dot_f32`
   waiting on memory); Elasticsearch's optimized scalar quantization ranks the one-bit estimates
   better (top 30 by estimate holds 0.877 of the true top 10 against 0.800) but with the whole
   beam rescored, as roll does, its 1M graphs lose 10%-filter recall (0.903 → 0.884). Left: a
   narrower rescore (roll still rescores the whole beam; it no longer costs measurable CPU, only
   resident float pages) needs an estimator that keeps filtered recall, and the mapping's
   default `rescore_vector.oversample` isn't applied to queries (no change to hits while every
   candidate is rescored); float construction on CPUs without AVX-512 downclocking.
   Decided 2026-11-17: concurrent knn segments stay on (lower latency for 3-25% more CPU on
   unmerged indices). Round 12 (docs/benchmarks/search.md) sizes each unmerged segment's beam to
   its share of the shard, as Lucene 10.5's optimistic collection does (a margin of 24 standard
   deviations, not Lucene's 16, which lost a few hits per 10,000), and searches a segment again
   with the full beam when its whole beam made the shard's best `num_candidates`: dense_vector
   `100-1000` on 4 segments per shard 43.7M → 29.4M instructions and 15.1 → 11.1 ms CPU per
   query, recall unchanged on Rally's queries and the 1M corpus, Rally's last vector loss now a
   tie. Only unfiltered searches over float and int8 fields narrow: filtered graph searches and
   one-bit estimates lost recall with narrow beams. Left: a filtered search's beam (a narrow
   one strands the filtered graph search), and the 6% of `100-1000` that zstd spends reading the
   hits' `_id`.
20. **The dashboard against Elastic Cloud and OpenSearch Dashboards** (landed: Snapshot and Restore
   in #325, Ingest Pipelines in #339, as Kibana's Stack Management has them; left: OpenSearch's
   snapshot policies, `fs` repositories' `path.repo`). Landed: the
   index browser and console on the per-index routes, an API keys panel in Settings, the mapping
   view as a field table, OpenSearch-flavor clusters' own clients and `OPENSEARCH_*` variables,
   and importing a file into an index from the Indices tab (Kibana's "Upload a file"): NDJSON, a
   JSON array or CSV with typed columns, previewed before anything is sent, read in 1 MiB chunks
   and sent as `_bulk` batches of at most 4 MiB through the index's own route, with progress,
   cancel and the first failures (100,000 ECS log lines, 42 MB, in 24 s on a debug build), and a
   new index from a field editor (types and their options, objects from dotted names,
   `dense_vector` or OpenSearch's `knn_vector`, `semantic_text` only on an endpoint the cluster can
   run: the default ELSER needs machine learning nodes; the request shown or opened in the
   console), and a time-based index (`@timestamp`, `timestamp`, or its only top-level date field)
   browsed as Discover does: range presets, documents over time (`auto_date_histogram`, counted
   from the points tree), a click on a bar narrows to it, newest first unless a column sorts; and
   a Data streams panel (backing indices, documents, size, the lifecycle's retention set in place
   or the ILM policy that manages the stream), and a profiled console search (`"profile": true`)
   drawn as Kibana's Search Profiler draws it (per shard: the query tree, collectors,
   aggregations and fetch with times and shares), and an Index templates panel (what each matches,
   its priority and what it sets; the built-in ones on request). On OpenSearch-flavor clusters a
   data stream's retention is set in the same place, as an ISM policy roll keeps for the stream
   (`roll-retention-<stream>`: roll over, then delete each backing index once it has been rolled
   over for the retention, so the write index is never deleted; new backing indices join through
   its `ism_template`). The console keeps each cluster's recent requests in the
   browser (credentials redacted), as Dev Tools keeps its history; named saves are left, low. Signed-in users who aren't admins see and manage their own clusters (#127), within their caps (#162). Also landed: a
   Snapshots tab, as Kibana's Snapshot and Restore does it: repositories (roll's own, managed by
   roll, plus `s3`, `fs` and `url`, and `gcs` and `azure` with #293, registered with their settings, verified and
   unregistered), snapshots (state, indices, size from `index_details`, duration, failed shards
   and failures; taken by hand, deleted), restores (all or picked indices, a rename pattern,
   aliases, global state, partial; a name that would land on an open index is caught before the
   request) followed through `_recovery` to the end, and snapshot lifecycle policies (schedule in
   words, retention, next run, last success and failure, run now, edit, and starting a stopped
   SLM), every request shown and a click from the console. Checked against Elasticsearch 9.5.5:
   SLM's readable times now carry milliseconds as `XContentBuilder.timeField` writes them
   (`2026-11-19T01:30:00.000Z`). Left: OpenSearch's snapshot management policies
   (`_plugins/_sm`) aren't served, so OpenSearch-flavor clusters take snapshots by hand or from an
   ISM `snapshot` action; their snapshots show no size (OpenSearch has no `index_details`); `fs`
   repositories need `path.repo`, which roll's clusters don't set.
   And a Pipelines tab, as Kibana's Ingest Pipelines does it: every pipeline with its processors
   and the indices whose `default_pipeline` or `final_pipeline` names it (built-in ones on
   request); a pipeline edited as a list of processors (the type, the options of the 26 types
   people use most, any other type's as JSON, `if`, `tag`, `ignore_failure`, `on_failure` per
   processor and for the pipeline, reordered in place) or as JSON a toggle away, every option it
   doesn't know kept; tested with a verbose `_simulate` on sample documents or five fetched from
   an index, read back per processor (ran, skipped, failed, failed and ignored, dropped; the
   fields each one changed) with the result before and after; and set as an index's
   `default_pipeline` or taken off it. Checked against Elasticsearch 9.5.5: a pipeline as `GET`
   gives it (with `created_date_millis`/`modified_date_millis`) now simulates as Elasticsearch's
   `Pipeline.create` reads it, and `PUT` refuses those dates as a `parse_exception`. Left: an
   OpenSearch-flavor cluster's `_nodes/ingest` lists Elasticsearch's processors, not OpenSearch's.
21. **Compatibility gaps** from the extended suites (5,083 of the 5,541 Elasticsearch passes, 5,244 in all, 46 suites at 100%; #187),
   largest first. Landed: ent-search's last 5 (a `rule` retriever over `rrf`/`linear`, inference
   inside a rule query's organic, workflow-restricted API keys), ES|QL's last 5 (`TOP_SNIPPETS`,
   `CATEGORIZE`, approximation's columns, `LIMIT n BY`, `unsigned_long` past `i64::MAX`),
   file-based service tokens, ES|QL's `METRICS_INFO` and `TS_INFO` (security's last 4, under
   field level security), and OpenSearch Security's `whoami`, `permissionsinfo`, `tenants`,
   `securityconfig`, `audit`, `ssl` and cache flush (`os_security_config.json`). Also landed: runtime
   fields' last tests and `logsdb`'s `_ignored_source` entries, byte for byte (#187). Decided
   2026-11-17: no skip index for `@timestamp` (measured: points answer a logs time window 4-223x
   faster on 2M sorted documents), so `logsdb`'s doc-values skipper test stays failing; and
   platinum's machine learning (278 tests: anomaly detection, data frame analytics, trained
   models) is not built now: semantic search through inference endpoints covers what people use
   it for in search, and the rest is a product of its own. Left: `_ignored_source`'s storage
   cost (its values are stored twice; measure on a logs corpus); downsample's last one (allocation filtering). In the opensearch dialect, pick
   `auto_date_histogram`'s rounding from the bounds as OpenSearch's rewrite does where it differs
   from collecting (roll counts with Elasticsearch's choice in both dialects). Landed: `rank_vectors`
   (#496: platinum 1,065 → 1,080).
22. **Language analysis: what's left.** Every analysis plugin Elastic Cloud ships runs now (42 of 42
   tests). Landed: `analysis-phonetic` (every encoder, Commons Codec's algorithms ported: identical
   codes to Java on 150,000 geonames names), `analysis-stempel` (Polish: Lucene's Egothor stemmer
   and table, identical on 121,000 words) and `analysis-ukrainian` (the Morfologik dictionary
   Elasticsearch ships, read in place; identical token streams on 21,000 lines; the three add 7.9 MB
   to the release binary, nothing to an idle resident set, 4 MiB once all are used), and the CJK
   plugins `analysis-kuromoji` (Japanese), `analysis-nori` (Korean) and `analysis-smartcn`
   (Chinese): Lucene's Viterbi search over IPADIC and mecab-ko-dic (search, normal and extended
   modes, decompounding, user dictionaries, n-best) and smartcn's HMM ported over Lucene's own
   dictionaries, with every tokenizer, filter, char filter and analyzer of the three plugins;
   tokens, offsets, positions and explain attributes identical to Elasticsearch's on 604 sentences
   per language (Tatoeba, with paragraphs past Lucene's 1,024-character buffer) in every mode and
   option, plus edge cases. The dictionaries (12.8 MB compressed in the binary) are decompressed
   only when an index first uses them: an idle node's resident set is unchanged, the first use adds
   24 MiB (Japanese), 38 MiB (Korean) and 5 MiB (Chinese). In-process throughput against Lucene
   10.5.1 on the same lines: Japanese and Korean at parity or ahead (`kuromoji_tokenizer` ~505k
   tokens/s against ~500k, `nori_tokenizer` ~295k against ~280k), `smartcn_tokenizer` behind (~390k
   against ~480k). 26 of 26 tests, listed in `_cat/plugins` and `_nodes/plugins`. And
   `analysis-icu`, 13 of 13: ICU4J 77.1's normalizer, break iterator and Thai, Lao, Khmer, Burmese
   and CJK dictionary engines ported over ICU's own data (every code point through all six
   normalization data sets in both modes, 20,000 random strings, 7,525 char filter lines with
   offsets and 4,579 lines of Thai, Lao, Khmer, Burmese, Chinese, Japanese, Korean and emoji text
   identical to ICU4J and Lucene), `icu_collation` and the `icu_collation_keyword` field on ICU4X's
   collator (orderings identical to ICU4J in 10 configurations over 20,000 words; key bytes differ
   above primary strength), `icu_transform` from rules, the built-in normalization, case, `Remove`
   and `Null` IDs, and the `icu_analyzer`; ICU's data (3.4 MB) is read in place: the release binary
   grows 5.3 MB, an idle `search-dev`'s resident set is unchanged (26 MiB) and using every ICU
   component adds 2.6 MiB. Left: ICU's data-driven transforms (`Any-Latin`, `Hiragana-Katakana`,
   `Han-Latin`, `Latin-ASCII`: ICU's transliteration rule data and its rule features beyond plain
   conversions), custom collation `rules` and `rule_files` for the tokenizer (both need ICU's rule
   compilers); then smartcn's speed.
23. **Elasticsearch's lifecycles: what's left.** Landed in #297: ILM's `shrink`, `downsample` and
   `searchable_snapshot` with Elasticsearch's step graph, `ilm-history-7`, and the data stream
   lifecycle's rounds; what follows is the state before it, kept for the record. The data stream lifecycle (rollover, retention,
   failure stores, the error store), ILM and snapshot lifecycle management (scheduled snapshots,
   retention, `.slm-history-7`, ILM's `wait_for_snapshot`) run now (`api/data_stream_lifecycle.rs`,
   `xpack/ilm_run.rs`, `xpack/slm.rs`; compatibility.md, "Known gaps"). They run time-based work
   at its due time and wake a sleeping cluster only for that. Left: ILM's `shrink` to fewer shards
   and `downsample` (roll has both APIs; the steps that swap the new index in aren't written),
   `searchable_snapshot`, and the `ilm-history-*` data stream.
   The data stream lifecycle's force merges and downsampling rounds aren't run either.
24. **Semantic search in roll's surfaces**: the dashboard's Inference tab lists a cluster's
   endpoints, creates them (OpenAI or any compatible URL, Cohere, Anthropic; the API key sent
   once, sealed, never shown back), tests them with a sample, deletes them, and opens a
   `semantic_text` quickstart in the console. The index browser's mapping view is a field table
   (type, inference endpoint, vector dims, similarity and index type, analyzers, what isn't
   indexed; the JSON a toggle away). An API-keyed endpoint's key can be a roll secret of the project
   whose canvas holds the cluster ("A roll secret" in the tab, `roll search inference create --secret`):
   rolld's `POST /v1/search/clusters/{id}/inference/secrets` reads it as the caller (`reveal`),
   creates the endpoint with it, and makes a `search-inference` sync, so a rotated value is pushed
   to the endpoint (`_inference/{task}/{id}/_update`) within seconds; the sync pins where the
   endpoint sends the key, a digest of its settings, and follows the cluster's id through a
   restore under another name (docs/secrets.md, "roll search's inference endpoints"). Left: the
   same for `s3` snapshot repositories' credentials (item 9); an endpoint whose key follows a
   secret made from the Elasticsearch API (it stays
   as Elasticsearch has it: clients send keys); and MCP tools for the new route (they follow the
   routes; the MCP thread).
25. **Semantic search** (the largest gap against Elasticsearch 9 and Elastic Cloud after machine
   learning). Landed in #265: the rest of Elasticsearch 9's inference services (Azure OpenAI, Amazon
   Bedrock, Google Vertex AI and AI Studio, Hugging Face, Mistral, Voyage AI, Jina AI, DeepSeek,
   watsonx, Alibaba Cloud, SageMaker and `custom`), each matched byte for byte against Elasticsearch's
   upstream requests; roll had `openai`, `cohere` and `anthropic` only, so anyone on Azure, AWS or
   Google couldn't use their own models. Landed (compatibility.md, "Inference" and "`semantic_text`"): the `_inference` API
   (`openai` with `url`, `cohere`, `anthropic`; keys sealed, `roll_net::PublicOnly`; `rate_limit`
   enforced; the preconfigured endpoints as a cluster without ML nodes; the test plugin's services
   on YAML test clusters) and `semantic_text` fields (embedded at index time in batches, chunked,
   stored as `_inference_fields`, `copy_to` sources; `semantic`, `match`, `knn`, `sparse_vector`
   and `exists` on them in `_search`, `_msearch` and templates; the semantic highlighter; `rrf`
   and `linear`'s simplified form; `text_similarity_reranker`). Left: `semantic_text` as a
   multi-field, `index_options` validation, the `embedding` task type and `semantic` field type,
   `random_reranker`, rerankers inside compound retrievers (landed 2026-11-14; the rest is low
   priority).
26. **Integration with the other threads**: on main since #29: previews branch search clusters
   (`scripts/preview-cost.sh`: the branch reads the parent's document, its writes stay in it, the
   service gets its `ELASTICSEARCH_URL`, landing deletes it; previews mask search fields), and
   roll secrets' search link asks for `access: "data"` keys and refuses anything else (#97). On
   main since #100: apps services link search clusters through roll secrets' search provider like
   buckets and streams (rolld's `secrets::link_provider("search")`, search's keys route with
   `access: "data"`): an API key with a data-only role, `ELASTICSEARCH_URL` +
   `ELASTICSEARCH_API_KEY`, or on an OpenSearch-flavor cluster an internal user with a data-only
   role of its own, `OPENSEARCH_URL`, `OPENSEARCH_USERNAME` and `OPENSEARCH_PASSWORD`; never the
   cluster's password (checked live 2026-11-16: relinking keeps the credential, unlinking revokes
   it, a prefix applies); on a fleet `Control::call` sends search's own in-process requests to the
   cluster's owner (`cluster::to_owner`). Next: an inference endpoint's key from a roll secret (#134, landed), credentials as `roll://ENV/KEY` references read through roll
   secrets' `resolve_for` (#329) for snapshot repositories and connectors, rebuilt when the
   secret's digest moves, roll.toml declaring a search link (apps' side), and checking search's routes through
   MCP's search mode (#80) end to end. A flavor switch doesn't carry keys across (an API key isn't
   an OpenSearch user): links made before one are reissued. The fleet test with every product on
   passed (2026-11-13: search 8,077 writes acknowledged and verified, no errors, longest stall
   1.2 s in the rolling restart).
   Joins the 2026-11-21 category review found, not yet built: an apps link can't be narrowed to
   index patterns or read-only (`[databases] kind = "search"` takes neither; the keys route does);
   apps' log drains can't yet land in a roll search data stream (apps is building a drain that writes
   ECS documents through `_bulk` with a key limited to `create_doc` on its stream: no OTLP needed); previews
   put a branch's superuser login in `ELASTICSEARCH_URL` beside the data key (fixed in #607: only the
   data credential, no fallback, and old branches remade with the key revoked and the password
   rotated first);
   a cluster's admin password can't be kept as a project secret from roll's API or CLI; and only a
   unit test covers previews' search branch, and one MCP test search (no create → bulk → query
   flow with a person's scoped token). Project members (roll secrets' #501, on main since
   991d98225): search's API serves a signed-in person only the clusters they own (`SignedIn` is
   the cluster's `owner`), so a member of a project whose canvas holds a cluster gets nothing from
   `/v1/search` or the dashboard's Search pages, and `remotes::may_link` compares owners; clusters
   on a project should follow the member's role through `Workspace::trusts`, as secrets and apps
   do. A member already gets trust in a cluster through its project home (search is one of
   roll-workspace's home kinds, and a cluster made through a project gets that project as its home),
   so roll secrets' links work for members; search's own API is what's left.
27. **Fleet takeover after a SIGKILL**: the fleet test's longest search stall (11.1 s at a
   SIGKILL) is the shared lease TTL (`ClusterConfig::lease_ttl`, 10 s: redis 10.4, nosql 10.6 in
   the same run) plus ~1 s to load the cluster from S3 and replay its WAL. Search's part is the
   load; the TTL is roll cloud's fleet-wide choice.
28. **CI**: the merge gate runs `search-codec`'s lib tests (`rust (search)`, 54 s on PR #29 with
   main's cache: under the minute, no room for another crate). (`search-store`'s mass-recovery
   WAL test flaked on a loaded host: a replay counted itself in the shared replay cache only
   after listing, so one could finish and empty it while the others listed; it now counts first.
   12 of 40 runs on one CPU failed before, 0 of 40 after.) `search-server`'s parked-governor
   test could spin forever when another test's request woke its parker first (it shared the
   process-wide flag): it has its own state and a deadline now (#181). Timing tests assert work,
   not wall time: linear work by CPU time at n and 8n (`search_test_support::assert_linear`),
   bounded work by the thread's CPU time (`cpu_time`), and waits as deadlines that fail with
   what they saw. `search-store`'s cache test waits for a finished download's task to let go of
   its map before each budget check (a reader returns before it does, and until then eviction
   skips the file: a 50 ms hold injected there failed the old test 20 of 20, the new one 0 of
   20), and its progressive download test counts the parts that landed before a read instead of
   timing it (14 of 20 runs failed on one CPU next to busy loops, 0 of 20 after).
   **Known flake under extreme load**: the extended `transform` suite's `transforms_start_stop`
   "start already started transform" expects a second `_start`, sent right after the first,
   to be refused. Elasticsearch 9.5.5 refuses it only while the batch transform's task exists:
   68-201 ms after the first `_start` answered (an empty or five-document source, load 55;
   accepted from 300 ms). Roll stopped it 26-83 ms after the answer, which a client slowed by
   load missed (one run in three at load 35). A batch transform now stays started for 200 ms
   after `_start` answers, or as long as its `_start` took on a slower node (`batch_hold_ms`:
   Elasticsearch's window is the same kind of cluster work, and stretches with load alike):
   196-229 ms measured, refused at 150 ms and accepted at 300 as on Elasticsearch. With the
   suite's search-dev processes and runner pinned to 2 CPUs next to 6 busy loops, a request can
   take over a second, and the test still lost 1 run in 10 (at host loads 63-81; the old build
   passed 10 of 10 there, but at loads 18-50). The suite's minimum (108) holds on a rerun, and
   that load is much harsher than CI's.
29. **Security**: landed in #632 (53a45bd36), a fix for a bypass a #358 review found: a role's document level
   security filter was evaluated with the request's own `runtime_mappings` in scope, so a runtime
   field could shadow the field the filter tests. Security filters (roles, API keys' role
   descriptors, OpenSearch Security's `dls`, a browser key's `filter`) are now built without the
   request's runtime fields on every read path, and a runtime field without a script reads only the
   `_source` its reader may see. #601 (in review) carries the whole-product
   review's fixes. A third, whole-product review on 2026-11-20 (main at 21708aef1) found no
   cross-tenant reach through branches, links or internal calls. Being fixed: Painless memory per
   execution, aggregation memory across the node (#549), observations keyed by name (a reused
   cluster name saw the old one's slow log), a node-wide body budget, and the console answering
   credentials under `admin`. Before it, a second review of what landed since the first (the `_inference` API, `semantic_text`,
   the new retrievers, workflow-restricted keys, service tokens, OpenSearch Security's new
   endpoints, ES|QL's new functions) landed in 7288684b9: `semantic_text` under field level
   security (queries and the semantic highlighter, `copy_to` sources included), an inference key
   given already sealed is sealed again (it was opened and sent to the client's `url`; watches
   already refuse one, connector and Fleet secrets always seal), and chunking, the semantic
   highlighter and `TOP_SNIPPETS` are linear in the text. Sealing binds the kind, not the
   cluster: branches copy a cluster's state under a new id. The two low leftovers are closed: a
   node serving many clusters reads `<path.conf>/service_tokens` only for the clusters its
   operator names in `roll.security.service_token_file.clusters` (the file is the node's, so a
   token in it signed in as `elastic/fleet-server` on every tenant's cluster; a dev-mode node
   serves one cluster and keeps Elasticsearch's behaviour; `search-server` `tests/http.rs`
   `the_nodes_service_token_file_signs_in_only_to_the_clusters_it_names`), and an inference
   call waits at most the node's `roll.inference.max_timeout` (default 120s) whatever `timeout`
   it asks for, retries and the rate limiter's wait included (Elasticsearch waits as long as
   asked; compatibility.md, "Inference"; `search-es` `tests/inference.rs`
   `a_request_waits_no_longer_than_the_nodes_cap_whatever_timeout_it_asks_for`). `roll.security.*`
   node settings are now kept out of `_cluster/settings?include_defaults` as node info already
   kept them. (A simplified `rrf`/`linear` now expands only against indices the caller may read.)
   A third review, of the analysis plugins, found five ways for one tenant to crash or exhaust a
   shared node, and all five are fixed. A deeply nested `UnicodeSet` overflowed the stack: sets
   now nest at most 101 deep, ICU's limit, checked before ICU4X parses them. Beider-Morse
   recursed once per `d'`/prefix: it is now a loop with at most 16 splits over the first 255
   characters. `icu_transform` rule sets such as `a > |a` looped forever: they now stop at ICU's
   loop limit with its error. Kölner/Haase doubled their spellings per match: a token now has at
   most 256 codes. Malformed kuromoji user dictionaries panicked: they are now errors. The caps
   are in compatibility.md, under "Extended suites".
   `s3` repositories read a bucket the tenant writes, so they're bounded and checked like any
   peer's input: repository data stops at 256 MiB inflated (streamed), 8M parsed values (charged
   to the `request` breaker while parsed), 10,000 snapshots and 100,000 indices; a restored
   document's source stops at `http.max_content_length` and is charged to `inflight_requests`
   as its write was; listings stop at 10,000 pages, 1M objects or 64 MiB of keys. A restore
   checks index, alias and data stream names as creating them does, and global state as
   `PUT _cluster/settings` and the template, pipeline and stored script APIs do (templates among
   the snapshot's own). Left, low: a cluster whose `max_body_mb` is over the node's 100mb can
   write documents its restore from a bucket refuses.
   The second review's low finding on background jobs is fixed. Transforms, rollup jobs, watches,
   ILM and SLM ran as the user or API key that set them up, and nothing checked that principal
   again, so a revoked key's ILM delete policy outlived the key. Each run now checks the user or
   key first (`internal::principal_problem`). A gone, invalidated or expired key, or a gone or
   disabled user, fails the run without running anything. Elasticsearch keeps running these jobs
   (checked on 9.5.5); compatibility.md has the difference and what each job type does. The
   dashboard's file import keeps exact numbers and ids and reads files with bounded memory (#103).
   The review of per-person access (#127) found one hole, now closed. A scoped token allowed to
   create clusters in a project could create one with a `parent` outside that project. The new
   cluster is a branch, so the token could read any cluster's indices. That create now also needs
   `write` on the parent, as `…/{id}/branches` does. The rest holds:
   - Every `/v1/search/clusters/{id}/…` call checks the owner. It does so before the fleet
     forwards it, and again on the owner node, behind its gate.
   - Another person's cluster answers the same 404 as a missing one.
   - A cluster's credentials and keys name that cluster only. Remote clusters reach only
     another cluster of the same owner (cross-cluster search, above: refused for anyone else's
     as for a missing one, checked again on every request) or the cluster itself, and reindex
     from a remote is off: the whitelist is empty.
   - `fs` and `url` repositories need `path.repo`, which rolld doesn't set.
   - A person's canvas reaches only their own clusters (`Workspace::trusts`), on every read and
     issue.

   A create, or a branch whose name is taken, answers 409, so a person learns that a cluster of
   that name exists. That is accepted: names are host labels (`<name>.search.…`), global like S3
   bucket names, and the 409 shows nothing else about the cluster. Left, low: a person's
   `GET …/{id}` shows the node id their cluster is loaded on (`status.node`).
