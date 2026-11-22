# Search for a multi-tenant SaaS

Your customers each search their own data, and must never see another's. This guide compares the
three ways to lay tenants out in roll search, sets up keys that enforce the boundary, uses a
branch for staging, and connects a cluster to a roll apps service. About twenty minutes.

## Before you start

You need a cluster, its URL and a key, as in [the first query](README.md#the-first-query-in-a-minute).
Making keys with roles needs an admin key:

```sh
roll search clusters create app-search
roll search keys create app-search setup --access admin --expires-in 1d
export ELASTICSEARCH_URL=…        # printed by the commands above
export ELASTICSEARCH_API_KEY=…
```

The requests use the `es` helper from [the guides' README](README.md#a-shorter-way-to-send-requests).

## Choose a layout

| | An index per tenant | One shared index, filtered keys | A cluster per tenant |
| --- | --- | --- | --- |
| Boundary | index names in the key | a query in the key (document level security) | separate credentials and storage |
| Good for | tenants with their own mappings or big differences in size | many small tenants with one schema | few large tenants, or ones that need their own settings, allowlist or snapshots |
| Cost of a tenant | an index (its mapping and segments) | a few documents | a cluster, which costs only storage while idle |
| Watch out for | thousands of tiny indices | a missing filter in your own queries leaks nothing, but a missing `tenant_id` on a document hides it | the per-person cluster cap (below) |

Because idle clusters cost only storage, a cluster per tenant is a real option on roll. Each one
is `roll search clusters create <tenant>`, with its own URL and keys. The rest of this guide covers
the first two layouts, inside one cluster.

## An index per tenant

Give every tenant's index the same settings with a template:

```sh
es PUT /_index_template/tenants '{
  "index_patterns": ["tenant-*"],
  "template": {
    "mappings": {
      "properties": {
        "title": {"type": "text"},
        "body":  {"type": "text"},
        "updated_at": {"type": "date"}
      }
    }
  }
}'

es PUT '/tenant-acme/_doc/1?refresh=true' '{"title": "Q3 roadmap", "body": "Ship the importer", "updated_at": "2026-11-20"}'
```

A key for one tenant names its indices. A pattern covers indices you add later (`tenant-acme-archive`):

```sh
es POST /_security/api_key '{
  "name": "tenant-acme",
  "expiration": "30d",
  "role_descriptors": {
    "acme": {"indices": [{"names": ["tenant-acme", "tenant-acme-*"], "privileges": ["read", "write"]}]}
  }
}'
```

A request with this key that names `tenant-globex`, or `tenant-*`, gets a 403 or sees only
`tenant-acme`'s indices.

## One shared index with filtered keys

Every document carries its tenant:

```sh
es PUT /docs '{
  "mappings": {
    "properties": {
      "tenant_id": {"type": "keyword"},
      "title": {"type": "text"},
      "body":  {"type": "text"}
    }
  }
}'

es PUT '/docs/_doc/acme-1?refresh=true' '{"tenant_id": "acme", "title": "Q3 roadmap", "body": "Ship the importer"}'
es PUT '/docs/_doc/globex-1?refresh=true' '{"tenant_id": "globex", "title": "Q3 roadmap", "body": "Hire two engineers"}'
```

The key carries the filter as a role's `query` (document level security). Every search, get,
aggregation and count made with it only sees matching documents, whatever the request says:

```sh
es POST /_security/api_key '{
  "name": "tenant-acme",
  "expiration": "1h",
  "role_descriptors": {
    "acme": {"indices": [{"names": ["docs"], "privileges": ["read"], "query": {"term": {"tenant_id": "acme"}}}]}
  }
}'
```

Try it: with the new key, `{"query": {"match": {"title": "roadmap"}}}` on `docs` returns only
`acme-1`. To hide fields as well, add `"field_security": {"grant": ["title", "body"]}` next to the
query.

The filter holds whatever body the client sends. A request's own `runtime_mappings` can't shadow
a field the filter tests, because the filter is evaluated without them. For a key a web page ships,
use a browser key with the same filter ([site-search.md](site-search.md), "Search from the
browser"); it adds rate limits and refuses writes and expensive queries.

## Make keys from your backend

Mint a short-lived key per tenant (or per user session) and cache it until it expires:

```python
import os
from elasticsearch import Elasticsearch

# The admin key, from a roll secret; only the backend has it.
admin = Elasticsearch(os.environ["ELASTICSEARCH_URL"], api_key=os.environ["SEARCH_ADMIN_KEY"])

def tenant_key(tenant: str) -> str:
    k = admin.security.create_api_key(
        name=f"tenant-{tenant}",
        expiration="1h",
        role_descriptors={tenant: {"indices": [{
            "names": ["docs"], "privileges": ["read"],
            "query": {"term": {"tenant_id": tenant}},
        }]}},
        metadata={"tenant": tenant},
    )
    return k["encoded"]

acme = Elasticsearch(os.environ["ELASTICSEARCH_URL"], api_key=tenant_key("acme"))
print(acme.search(index="docs", query={"match": {"title": "roadmap"}})["hits"]["hits"])
```

A key made with an API key is limited by that key's own permissions, so it can never do more than
the admin key. Revoke one at once with `DELETE /_security/api_key` and `{"ids": ["<id>"]}`, or every
key of a tenant with `{"name": "tenant-acme"}`.

`encoded` works against the cluster's own host (`https://app-search.search.…`). On a shared address
such as `http://127.0.0.1:59200`, encode `<id>@app-search:<api_key>` in base64 instead, so the
address knows which cluster the key belongs to.

## Stage changes on a branch

A branch is an instant copy of the cluster, with its own password, that shares unchanged data
with its parent:

```sh
roll search clusters branch app-search app-search-staging
```

Try a new mapping, an analyzer or a reindex there, point staging at its URL, and delete it with
`roll search clusters delete app-search-staging` when you're done. Writes to the branch never reach
the parent. A branch doesn't run its parent's snapshot policies or watches until you put them
again, so it won't send a parent's webhook twice. `--at 2h` copies the cluster as it was two hours
ago, which is also a quick way to look at data before a bad import.

## Connect a roll apps service

roll secrets can hand a service the cluster's URL and an API key of its own, rotated by roll and
never the cluster's password:

1. In the dashboard, put the search cluster and the service on the same project's canvas.
2. Open the project's secrets. The overview suggests `ELASTICSEARCH_URL` and
   `ELASTICSEARCH_API_KEY` for the cluster in each environment; accept them. The key is a
   `data` key: it can read and write indices, templates, pipelines and lifecycle policies, but
   not security.
3. Deploy the service. Its environment's secrets are in its environment variables:

```js
import { Client } from '@elastic/elasticsearch'

const es = new Client({
  node: process.env.ELASTICSEARCH_URL,
  auth: { apiKey: process.env.ELASTICSEARCH_API_KEY },
})
```

A `data` key can't create keys. If the service mints tenant keys, make an admin key and store it
as a secret yourself:

```sh
roll search keys create app-search tenant-minter --access admin
roll secrets -p my-project -e production set SEARCH_ADMIN_KEY     # paste the api_key on stdin
```

An OpenSearch-flavor cluster gets `OPENSEARCH_URL`, `OPENSEARCH_USERNAME` and
`OPENSEARCH_PASSWORD` instead. If the cluster has an IP allowlist, check
[../architecture.md](../architecture.md), "Client addresses and IP allowlists": roll apps send
from addresses an allowlist may not admit.

## Set per-cluster limits

```sh
roll search clusters update app-search --max-in-flight 64 --max-body-mb 20 --idle 900
roll search clusters update app-search --allow 203.0.113.0/24=office --allow 198.51.100.7
```

- `--max-in-flight`: requests running at once; more wait or get 429.
- `--max-body-mb`: the largest request body (`http.max_content_length`).
- `--idle`: seconds without requests before the cluster unloads; `0` keeps it loaded.
- `--allow`: only these networks reach the cluster's endpoint (`--allow-any` clears the list).
  The dashboard and `roll search` aren't filtered.

A person may own 50 clusters, branches included, and 2 of them may be always-on (`--idle 0`).
Past that, a create, branch or update answers 409 naming the limit. These are the node's defaults,
which the operator can change (`--search-max-clusters-per-person`,
`--search-max-always-on-per-person`).

## Next steps

- What each tenant searches for: turn on analytics per index
  ([site-search.md](site-search.md#suggest-queries-from-what-people-search)); with an index per
  tenant, each tenant's queries stay separate.
- Semantic search inside the tenant boundary: [rag.md](rag.md). A filtered key's semantic queries
  only reach its tenant's documents.
- Usage per cluster: `roll search clusters usage app-search`.
