# roll search

Elasticsearch-compatible search clusters that cost nothing while idle, wake in milliseconds,
branch instantly and keep their data on object storage. Clients speak the Elasticsearch REST API
(or OpenSearch's): the official clients, searchkick, Laravel Scout and Spring Data work by
changing the URL. Semantic and hybrid search work as on Elasticsearch 9: `semantic_text` fields
embed through the model provider you configure (`_inference`: OpenAI or any compatible server,
Cohere, Anthropic; the key sealed in the cluster), and `rrf`/`linear` retrievers mix them with
lexical search.

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
| `--search-max-clusters-per-person` (`ROLL_SEARCH_MAX_CLUSTERS_PER_PERSON`) | `50` | clusters a signed-in person may own, branches included (0: no cap; the operator and admins aren't capped) |
| `--search-max-always-on-per-person` (`ROLL_SEARCH_MAX_ALWAYS_ON_PER_PERSON`) | `2` | of those, always-on ones (`idle_timeout_s` 0) (0: no cap) |
| `--search-audit-retention-days` (`ROLL_SEARCH_AUDIT_RETENTION_DAYS`) | `7` | how long each cluster's security audit log keeps its entries |
| `--search-audit-max-mb` (`ROLL_SEARCH_AUDIT_MAX_MB`) | `32` | audit entries kept per cluster, compressed MiB; past it the oldest go |
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

Semantic search (or the dashboard's Inference tab):

```sh
OPENAI_API_KEY=… roll search inference create app emb --model text-embedding-3-small   # key from the environment, sealed in the cluster
roll search request app PUT /articles '{"mappings":{"properties":{"body":{"type":"semantic_text","inference_id":"emb"}}}}'
roll search request app POST '/articles/_doc?refresh=true' '{"body":"Passwords can be reset from the sign-in page."}'
roll search request app POST /articles/_search '{"query":{"semantic":{"field":"body","query":"forgot my password"}}}'
roll search inference list app | test app emb "some text" | delete app emb
```

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
- Nothing in the API is node-wide besides `GET /v1/search`'s figures: node settings and the
  default repository are rolld's flags (`--search-*`), not routes. A cluster's data plane is
  unchanged: the REST port with the cluster's own credentials, which give what any tenant gets.

`crates/search-control/tests/control.rs` `people_see_only_their_own_clusters` and
`people_are_held_to_their_cluster_caps` cover it.

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
| GET | `/v1/search/clusters/{id or name}` | settings, `status` (`loaded` with live index count and requests in flight, and live `docs` once every index is open, or `asleep` with `wakes_at`, unix seconds, when a background job is due to wake it), last metered `indices`/`docs`/`stored_bytes` and `own_bytes` (what a branch references that its parent's latest commit points don't; all of `stored_bytes` for a cluster that isn't a branch), `url`, `branches` |
| PATCH | `/v1/search/clusters/{id}` | `flavor`, `idle_timeout_s` (0: always loaded), `max_in_flight`, `max_body_mb`, `destructive_requires_name`, `ip_allowlist` (below); the idle timeout, body limit, `max_in_flight` and allowlist apply at once (also to a cluster that is waking up); a new flavor or `destructive_requires_name` needs a reload: the cluster is put to sleep now, or as soon as no request runs (always-on clusters included), and keeps its current ones until then |
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
with the key from `$ROLL_SEARCH_API_KEY` (OpenSearch-flavor clusters: OpenSearch's clients and
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
storage is billed until it is purged, when its rows go. Prices are AWS list prices in `crates/search-control/src/pricing.rs`, each
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

Built 2026-11-19 (in review): Terraform for search clusters (#350's item 3): clusters, API keys
(also ephemeral), snapshot repositories and a cluster data source in `sdk/terraform-provider-roll`
("Terraform" above); remote-cluster links and allowed origins follow once they're on main.

1. **Search from the browser** (in progress; Algolia's search-only and secured API keys,
   Typesense's scoped keys, Meilisearch's tenant tokens, Elastic's search applications): a web
   page can't call a roll cluster today. There's no CORS (`http.cors.*` are static settings no
   tenant can set), so every site needs a backend in front of search, which is the first thing
   those products remove. Per-cluster allowed origins (set from roll's API and the dashboard, as
   Elastic Cloud's user settings set `http.cors.*`), keys a page can ship (search-only, limited
   to indices or a search application, filters baked in, an expiry, a rate limit per key),
   Elasticsearch's `_application/search_application/{name}/_search` as the safe surface, and a
   quickstart that works with Elastic's Search UI and InstantSearch through Searchkit. An IP
   allowlist still applies to browsers; the docs say how the two combine.
2. **The Rally losses left by the full rerun** (Round 10, vectors, #268: Elasticsearch 9.1+ runs no merge while less than ~93.5 GB of disk is free, and this host was 91-98% full, so Round 9's Elasticsearch races were held on vectors and noaa (4 of 6); the harness now sets `indices.merge.disk.check_interval=0`. With merges flowing, the force-merged vector losses are gone and dense_vector `100-1000_multiple_segments` (2.30x) is the one loss with ranges apart. Round 11, noaa and nyc_taxis rerun with merges flowing at `769dd6744` (#340): Round 9's noaa claims held (every range-field query ahead in median, all ties; `conjunction_big_range_big_term` 7.4 against 12.2 ms, faster in 6 of 6 rounds; indexing 0.75, ahead in 6 of 6), and nyc_taxis wasn't held at all (its 0-merge races are Elasticsearch's merge policy). What changed: nyc_taxis `distance_amount_agg` leads by less (46 against 78 ms, 0.59; Round 9's 118 was load), its `range` now leads (0.88), and Elasticsearch now indexes nyc_taxis faster in median (1.03 docs/s, bulk p50 1.04, ties)) (docs/benchmarks/search.md: Round 8, every Rally
   track against Elasticsearch 9.5.5 and OpenSearch 3.9 at `83fac5982`; Round 9, the four tracks
   #29 fixed rerun at `c2b47b251`, three alternating rounds each, noaa six; Round 11, noaa and
   nyc_taxis again at `769dd6744`). Roll leads most tasks
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
   8-15% to code placement; kept for a retry with placement controlled); nested `randomized-sorted-term-queries` 1.23x OpenSearch
   (12.6 against 10.2 ms, ranges apart; Round 8 1.62x; fixed in #125, which skips the parents no
   child of which can beat the queue: 3.5-4.3 against OpenSearch's 8.5-9.7 ms on the same CPUs,
   a third of its server CPU, identical hits; Rally rerun pending); nested `nested-date-histo` 1.17x
   Elasticsearch (tie); the nested bulk p50 1.16x (tie, docs/s ahead); http_logs
   `sort-size-desc-after-force-merge-1-seg` 1.12x OpenSearch (Round 8, tie); nyc_taxis' bulk p50
   1.04x Elasticsearch (Round 11, tie; slower in 3 of 3 rounds, 1.10 at equal foreign load) and its
   docs/s 1.03x (tie, undecided at equal load); http_logs `scroll` 1.03x OpenSearch and the pmc and
   so bulk p50s (Round 8, ties). Round 9's noaa `range_field_disjunction_big_range_small_term_query`
   1.06x and nyc_taxis `range` 1.05x are gone in Round 11 (0.93, a tie; 0.88). Rerun next: so_vector with #156, http_logs (its indexing regression
   since `dbe79f8f7` is fixed by `6c2834a3e`, not yet measured under Rally), so, pmc and
   percolator at the current head. Not a loss: `terms_enum` (OpenSearch has no such API; its
   "0.57 ms" was an error). Still open from before: one-client indexing CPU (51.8 against 34.6
   µs/doc on the ECS log), BP only for time-series and log indices, the request path's
   allocations, one allocator policy.
3. **Connectors that sync** (built in #318: `postgresql` full and incremental, `s3`, sync rules,
   a Connectors tab; in review. Left: linking a person's own roll Postgres or bucket with a
   read-only credential of its own, credentials from roll secrets after #134, Postgres advanced
   rules, PDF and Office text; Elastic's connectors, Algolia's and Typesense's data sync):
   roll serves Elasticsearch's `_connector` API but runs no syncs, so getting a database or a
   bucket into an index is the user's own job. A runner on the cluster's owner node, scheduled
   like ILM (an idle cluster pays nothing between syncs), with `_connector/_sync_job` rows as
   Elasticsearch keeps them; `postgresql` (full and incremental) and `s3` first, reaching a
   person's own roll Postgres or storage through a link instead of copied credentials.
4. **Cross-cluster search between a person's own clusters** (in progress; Elastic Cloud's remote clusters
   between deployments; OpenSearch's cross-cluster search): roll's remote clusters reach only the
   cluster itself today. Same owner only, trusted explicitly, keys scoped to the remote, on the
   fleet through the owner's signed forward; `_resolve/cluster` and `_remote/info` as
   Elasticsearch answers them.
5. **Snapshots on the fleet.** Landed: the `gcs` and `azure` repository types (#293); and every cluster whose data is in object storage (the fleet
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
   credentials from roll secrets instead of the repository's settings. Restoring from a snapshot
   Elasticsearch wrote: feasible but not built. It means reading Elasticsearch's repository
   (`index-N`, `snap-*.dat`, `meta-*.dat`, shard `index-*`/`snap-*.dat`: SMILE inside Lucene
   codec headers, DEFLATE-compressed) and each shard's Lucene files (segment infos, field infos,
   compound files, LZ4/DEFLATE stored fields for `_id`, `_routing`, `_source`, numeric doc values
   for `_seq_no`, `_primary_term`, `_version`, live docs, flat vectors for excluded source
   vectors; Lucene 9 and 10 codecs for 8.x and 9.x snapshots), all as hostile input, then
   re-indexing every document from `_source`: about 4,000 lines and Elasticsearch-made fixtures,
   and a restore that is a full reindex. Indices without a stored `_source` (synthetic source,
   the logsdb and time-series default, `_source.enabled: false`) can't come back that way.
   Decided 2026-11-17: not now. A live cluster moves by reindex from remote; build it once tenants
   ask to move by backup alone.
6. **Vector search's footprint** (docs/benchmarks/search.md, "Vector search": 1M × 384 against
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
   unmerged indices); the vector rerun under Rally (in progress) decides what to cut next.
7. **The dashboard against Elastic Cloud and OpenSearch Dashboards** (in progress: Snapshot and
   Restore, then Ingest Pipelines, as Kibana's Stack Management has them). Landed: the
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
8. **Compatibility gaps** from the extended suites (5,083 of the 5,541 Elasticsearch passes, 5,244 in all, 46 suites at 100%; #187),
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
   from collecting (roll counts with Elasticsearch's choice in both dialects).
9. **Language analysis: what's left.** Every analysis plugin Elastic Cloud ships runs now (42 of 42
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
10. **Elasticsearch's lifecycles: what's left.** Landed in #297: ILM's `shrink`, `downsample` and
   `searchable_snapshot` with Elasticsearch's step graph, `ilm-history-7`, and the data stream
   lifecycle's rounds; what follows is the state before it, kept for the record. The data stream lifecycle (rollover, retention,
   failure stores, the error store), ILM and snapshot lifecycle management (scheduled snapshots,
   retention, `.slm-history-7`, ILM's `wait_for_snapshot`) run now (`api/data_stream_lifecycle.rs`,
   `xpack/ilm_run.rs`, `xpack/slm.rs`; compatibility.md, "Known gaps"). They run time-based work
   at its due time and wake a sleeping cluster only for that. Left: ILM's `shrink` to fewer shards
   and `downsample` (roll has both APIs; the steps that swap the new index in aren't written),
   `searchable_snapshot`, and the `ilm-history-*` data stream.
   The data stream lifecycle's force merges and downsampling rounds aren't run either.
11. **Semantic search in roll's surfaces**: the dashboard's Inference tab lists a cluster's
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
   same for `s3` snapshot repositories' credentials (item 5); an endpoint whose key follows a
   secret made from the Elasticsearch API (it stays
   as Elasticsearch has it: clients send keys); and MCP tools for the new route (they follow the
   routes; the MCP thread).
12. **Semantic search** (the largest gap against Elasticsearch 9 and Elastic Cloud after machine
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
13. **Integration with the other threads**: on main since #29: previews branch search clusters
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
   cluster's owner (`cluster::to_owner`). Next: an inference endpoint's key from a roll secret (#134,
   approved by the secrets thread), credentials as `roll://ENV/KEY` references read through roll
   secrets' `resolve_for` (#329) for snapshot repositories and connectors, rebuilt when the
   secret's digest moves, roll.toml declaring a search link (apps' side), and checking search's routes through
   MCP's search mode (#80) end to end. A flavor switch doesn't carry keys across (an API key isn't
   an OpenSearch user): links made before one are reissued. The fleet test with every product on
   passed (2026-11-13: search 8,077 writes acknowledged and verified, no errors, longest stall
   1.2 s in the rolling restart).
14. **Fleet takeover after a SIGKILL**: the fleet test's longest search stall (11.1 s at a
   SIGKILL) is the shared lease TTL (`ClusterConfig::lease_ttl`, 10 s: redis 10.4, nosql 10.6 in
   the same run) plus ~1 s to load the cluster from S3 and replay its WAL. Search's part is the
   load; the TTL is roll cloud's fleet-wide choice.
15. **CI**: the merge gate runs `search-codec`'s lib tests (`rust (search)`, 54 s on PR #29 with
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
16. **Security**: a second review of what landed since the first (the `_inference` API, `semantic_text`,
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
   - A cluster's credentials and keys name that cluster only. Remote clusters reach only the
     cluster itself, and reindex from a remote is off: the whitelist is empty.
   - `fs` and `url` repositories need `path.repo`, which rolld doesn't set.
   - A person's canvas reaches only their own clusters (`Workspace::trusts`), on every read and
     issue.

   A create, or a branch whose name is taken, answers 409, so a person learns that a cluster of
   that name exists. That is accepted: names are host labels (`<name>.search.…`), global like S3
   bucket names, and the 409 shows nothing else about the cluster. Left, low: a person's
   `GET …/{id}` shows the node id their cluster is loaded on (`status.node`).
