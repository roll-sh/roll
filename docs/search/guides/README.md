# roll search guides

roll search runs Elasticsearch-compatible search clusters. A cluster sleeps when nobody uses
it, wakes on the next request in milliseconds, and keeps its data in object storage, so an idle
cluster costs only storage. Clients speak the Elasticsearch 9 REST API (or OpenSearch 3's), so the
official clients and the tools built on them work by changing the URL and the key.

These guides are for people building on roll search. The operator's and internals' view is
[../README.md](../README.md); what roll serves, request by request, is
[../compatibility.md](../compatibility.md).

## Pick a guide

| You are building | Guide | What it covers |
| --- | --- | --- |
| A search box for a docs site or blog | [site-search.md](site-search.md) | indexing pages from a sitemap, search as you type, typos, highlights, synonyms, suggestions |
| A product catalogue | [ecommerce.md](ecommerce.md) | product mappings, facets, sorting, popularity, pinning and hiding products, click analytics |
| Log search | [logs.md](logs.md) | data streams, retention, ingest pipelines, ES\|QL, snapshots |
| Retrieval for an LLM app | [rag.md](rag.md) | `semantic_text`, semantic and hybrid search, inference endpoints, a retrieve-then-generate script |
| A multi-tenant SaaS | [saas-tenants.md](saas-tenants.md) | an index per tenant or filtered keys, staging branches, roll apps, caps |

Start with the first query below if you have no cluster yet.

## The first query in a minute

### Create a cluster

In the dashboard: open **Search**, click **New cluster**, type a name (lowercase letters, digits
and `-`), keep the API on `elasticsearch`, and click **Create**. The panel that opens shows the
URL, the username and the password. The password is shown once: copy it now.

From a terminal, sign the CLI in once, then create the cluster:

```sh
roll login <your roll's URL>
roll search clusters create books
```

```text
url       https://books.search.us-east-1.roll.sh
username  books
password  …   (shown once)
```

The name is the cluster's identity: its username, the first label of its host name and
`cluster_name` in `GET /`. It can't be changed later.

### Or try it on sample data first

A new cluster's **Overview** offers two datasets roll generates for you: **Products** (1,000 items
of a made-up shop) and **Web logs** (2,000 log lines from the last 24 hours). Click **Load**, and
three ready queries run on it in place: search as you type with a typo, facets and a filtered sort
on the products; errors in the last hour, top paths and an ES|QL `STATS` on the logs (ES|QL only on
`elasticsearch` clusters). Each has **Open in Console** and **Copy as** curl, JavaScript or Python,
with the cluster's URL and a placeholder for the key. **Remove** deletes the index or data stream
again; the Indices tab's **Sample data** button brings the panel back any time.

### Get a key

The password can do everything, including managing users and keys. Give your code an API key
instead:

```sh
roll search keys create books quickstart --expires-in 30d
```

```text
key … (quickstart) of books, data access
api_key   …   (shown once)

ELASTICSEARCH_URL=https://books.search.us-east-1.roll.sh ELASTICSEARCH_API_KEY=…
```

A `data` key (the default) reads and writes indices, templates, ingest pipelines and lifecycle
policies, and can't touch security. `--access admin` makes a key that can do whatever the cluster's
password can; the guides use one for setup steps that need it (synonyms, query rules, inference
endpoints, snapshots, other keys). In the dashboard, keys are under the cluster's **Settings**,
**API keys**.

Export the two values. Every guide uses them:

```sh
export ELASTICSEARCH_URL=https://books.search.us-east-1.roll.sh
export ELASTICSEARCH_API_KEY=…
```

Lost them? `roll search clusters url books` prints the URL, `roll search keys list books` lists
keys, `roll search keys revoke books <id>` revokes one, and `roll search clusters password books`
rotates the password (the old one stops working at once).

### Index a document

```sh
curl -X PUT "$ELASTICSEARCH_URL/books/_doc/1?refresh=true" \
  -H "Authorization: ApiKey $ELASTICSEARCH_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"title": "Dune", "author": "Frank Herbert", "year": 1965}'
```

The index `books` is created on the first write, with a mapping guessed from the document.
`refresh=true` makes the document searchable before the call returns.

### Search it

```sh
curl "$ELASTICSEARCH_URL/books/_search" \
  -H "Authorization: ApiKey $ELASTICSEARCH_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"query": {"match": {"title": "dune"}}}'
```

The answer has one hit with `"_id": "1"`. That's the first query. The rest of this page shows the
same search from code.

### JavaScript

```sh
npm install @elastic/elasticsearch
```

```js
import { Client } from '@elastic/elasticsearch'

const client = new Client({
  node: process.env.ELASTICSEARCH_URL,
  auth: { apiKey: process.env.ELASTICSEARCH_API_KEY },
})

const result = await client.search({ index: 'books', query: { match: { title: 'dune' } } })
console.log(result.hits.hits)
```

### Python

```sh
pip install elasticsearch
```

```python
import os
from elasticsearch import Elasticsearch

es = Elasticsearch(os.environ["ELASTICSEARCH_URL"], api_key=os.environ["ELASTICSEARCH_API_KEY"])
result = es.search(index="books", query={"match": {"title": "dune"}})
print(result["hits"]["hits"])
```

### Go

```sh
go get github.com/elastic/go-elasticsearch/v9
```

```go
package main

import (
	"fmt"
	"os"
	"strings"

	"github.com/elastic/go-elasticsearch/v9"
)

func main() {
	es, err := elasticsearch.NewClient(elasticsearch.Config{
		Addresses: []string{os.Getenv("ELASTICSEARCH_URL")},
		APIKey:    os.Getenv("ELASTICSEARCH_API_KEY"),
	})
	if err != nil {
		panic(err)
	}
	res, err := es.Search(
		es.Search.WithIndex("books"),
		es.Search.WithBody(strings.NewReader(`{"query": {"match": {"title": "dune"}}}`)),
	)
	if err != nil {
		panic(err)
	}
	defer res.Body.Close()
	fmt.Println(res.String())
}
```

Java, Ruby, PHP, searchkick, Laravel Scout and django-elasticsearch-dsl work the same way. The
versions roll is tested with are in compatibility.md's "Client matrix".

## A shorter way to send requests

The guides send many small requests. Instead of the full `curl` line each time, define `es` once
per shell:

```sh
es() { # es METHOD PATH [BODY | @file]
  curl -sS -X "$1" "$ELASTICSEARCH_URL$2" \
    -H "Authorization: ApiKey $ELASTICSEARCH_API_KEY" \
    -H 'Content-Type: application/json' ${3:+--data-binary "$3"}
  echo
}

es GET '/books/_search?q=title:dune'
```

Each `es` line also works as `roll search request <cluster> METHOD PATH [BODY]`. That command goes
through roll's API with your roll login and runs as the cluster's owner, so it needs no key. The
dashboard's **Console** tab takes the same requests and completes paths, parameters and fields as
you type.

## OpenSearch flavor

If your code uses OpenSearch's clients, make an OpenSearch-flavor cluster: in the dashboard pick
`opensearch` as the API, or run

```sh
roll search clusters create books-os --flavor opensearch
```

It answers as OpenSearch 3.9 does. OpenSearch clients sign in with a username and password, so on
these clusters `roll search keys create` makes an internal user instead of an API key and prints
`OPENSEARCH_URL`, `OPENSEARCH_USERNAME` and `OPENSEARCH_PASSWORD`:

```python
import os
from opensearchpy import OpenSearch

client = OpenSearch(
    hosts=[os.environ["OPENSEARCH_URL"]],
    http_auth=(os.environ["OPENSEARCH_USERNAME"], os.environ["OPENSEARCH_PASSWORD"]),
)
print(client.search(index="books", body={"query": {"match": {"title": "dune"}}}))
```

The guides use Elasticsearch's APIs. Where OpenSearch has its own (Index State Management for
retention), the guide says so.

## Good to know

- A cluster that has been idle for a while (five minutes by default; `--idle` on create or
  `roll search clusters update`) unloads. The next request loads it again; you don't do anything.
- Branches are instant copies of a cluster with their own password:
  `roll search clusters branch books books-test`. Add `--at 1d` to copy it as it was a day ago.
- `roll search clusters usage books` shows what the cluster has cost, next to the same workload on
  Elastic Cloud and Amazon OpenSearch Service.
- If an Elasticsearch API answers differently from Elasticsearch 9.5.5, it's a bug unless
  compatibility.md lists it.
