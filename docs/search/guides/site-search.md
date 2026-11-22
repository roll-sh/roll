# Site search

A search box for a docs site, a blog or a help centre: results as the visitor types, typos
forgiven, matches highlighted, your own synonyms, and suggestions built from what people search
for. About fifteen minutes.

## Before you start

You need a cluster, its URL and a key, as in [the first query](README.md#the-first-query-in-a-minute).
Some steps here change the cluster's settings (synonyms, keys), so make a short-lived admin key
for the setup:

```sh
roll search clusters create docs
roll search keys create docs setup --access admin --expires-in 1d
export ELASTICSEARCH_URL=…        # printed by the commands above
export ELASTICSEARCH_API_KEY=…
```

The requests use the `es` helper from [the guides' README](README.md#a-shorter-way-to-send-requests).

## Create a synonyms set

Synonyms live in a set that analyzers read, so you can change them later without reindexing.
Create the set first; the index refers to it by name.

```sh
es PUT /_synonyms/site '{
  "synonyms_set": [
    {"id": "signin", "synonyms": "sign in, log in, login"},
    {"id": "k8s", "synonyms": "k8s => kubernetes"}
  ]
}'
```

Or keep them in a file, one Solr-format rule a line, and load it with the CLI:

```sh
printf 'sign in, log in, login\nk8s => kubernetes\n' > synonyms.txt
roll search synonyms put docs site synonyms.txt
```

## Create the index

```sh
es PUT /pages '{
  "settings": {
    "analysis": {
      "filter": {
        "site_synonyms": {"type": "synonym_graph", "synonyms_set": "site", "updateable": true}
      },
      "analyzer": {
        "search_with_synonyms": {"tokenizer": "standard", "filter": ["lowercase", "site_synonyms"]}
      }
    }
  },
  "mappings": {
    "properties": {
      "url":   {"type": "keyword"},
      "title": {"type": "search_as_you_type"},
      "body":  {"type": "text", "search_analyzer": "search_with_synonyms"}
    }
  }
}'
```

- `search_as_you_type` indexes the title with extra sub-fields (`title._2gram`, `title._3gram`,
  `title._index_prefix`) so partial words match quickly.
- The synonyms apply when searching `body` (`search_analyzer`), not when indexing it. That's what
  lets the set change without a reindex.

## Index your pages

This script reads your sitemap, fetches each page, keeps the title and the visible text, and
sends them to `pages` in bulk. It needs Python 3 and `pip install elasticsearch`.

```python
# index_sitemap.py: python index_sitemap.py https://example.com/sitemap.xml
import os, re, sys, urllib.request, xml.etree.ElementTree as ET
from html.parser import HTMLParser
from elasticsearch import Elasticsearch, helpers

es = Elasticsearch(os.environ["ELASTICSEARCH_URL"], api_key=os.environ["ELASTICSEARCH_API_KEY"])
SKIP = {"script", "style", "nav", "footer"}

class Page(HTMLParser):
    def __init__(self):
        super().__init__()
        self.title, self.text, self.skip, self.in_title = "", [], 0, False
    def handle_starttag(self, tag, attrs):
        self.skip += tag in SKIP
        self.in_title |= tag == "title"
    def handle_endtag(self, tag):
        self.skip -= tag in SKIP and self.skip > 0
        self.in_title &= tag != "title"
    def handle_data(self, data):
        if self.in_title: self.title += data
        elif not self.skip: self.text.append(data)

def fetch(url):
    req = urllib.request.Request(url, headers={"User-Agent": "site-search-indexer"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return r.read().decode("utf-8", "replace")

def pages(sitemap):
    ns = {"sm": "http://www.sitemaps.org/schemas/sitemap/0.9"}
    for loc in ET.fromstring(fetch(sitemap)).findall(".//sm:loc", ns):
        url = loc.text.strip()
        page = Page()
        page.feed(fetch(url))
        body = re.sub(r"\s+", " ", " ".join(page.text)).strip()
        yield {"_index": "pages", "_id": url, "url": url, "title": page.title.strip(), "body": body}

ok, errors = helpers.bulk(es, pages(sys.argv[1]), raise_on_error=False)
print(f"indexed {ok}, failed {len(errors)}")
```

The page's URL is its `_id`, so running the script again updates pages instead of duplicating
them. If your sitemap is an index of other sitemaps, run it once per child sitemap. Run it from
CI after each deploy, or on a schedule.

roll doesn't crawl sites for you yet (`## Next` item 13 in [../README.md](../README.md)). Syncing
pages from an S3 bucket is coming with the connectors (#318, in review); it isn't on main.

## Search as you type

Send this on every keystroke:

```sh
es POST /pages/_search '{
  "size": 5,
  "_source": ["url", "title"],
  "query": {
    "multi_match": {
      "query": "kubernetes instal",
      "type": "bool_prefix",
      "fields": ["title", "title._2gram", "title._3gram", "body"],
      "fuzziness": "AUTO"
    }
  }
}'
```

`bool_prefix` treats every word as a whole word except the last, which matches as a prefix, so
`instal` already finds "installing". For a single field, the `match_bool_prefix` query does the
same.

## Tolerate typos

`"fuzziness": "AUTO"` above lets each word differ by one edit when it has 3 to 5 characters and
by two edits when it's longer. Words of one or two characters must match exactly. The last word
of a `bool_prefix` query is a prefix and isn't fuzzy.

If typos in the first letter bring in noise, add `"prefix_length": 1`: the first character must
then match exactly.

## Highlight matches

```sh
es POST /pages/_search '{
  "_source": ["url", "title"],
  "query": {"multi_match": {"query": "log in", "fields": ["title", "body"], "fuzziness": "AUTO"}},
  "highlight": {
    "pre_tags": ["<mark>"], "post_tags": ["</mark>"],
    "fields": {"title": {}, "body": {"fragment_size": 150, "number_of_fragments": 2}}
  }
}'
```

Each hit has a `highlight` object with up to two 150-character passages of `body` with the
matched words wrapped in `<mark>`. Because of the `signin` synonym rule, "log in" also highlights
"sign in".

## Change synonyms

Replace or add one rule:

```sh
es PUT /_synonyms/site/k8s '{"synonyms": "k8s, kube => kubernetes"}'
```

Or replace the whole set with `PUT /_synonyms/site` (or `roll search synonyms put docs site
synonyms.txt`) as before. The answer says which analyzers it
reloaded; the next search uses the new rules. In the dashboard, the cluster's **Relevance** tab
edits synonym sets as rows or as text.

## Suggest queries from what people search

roll can build autocomplete suggestions from the searches people actually run. Turn on search
analytics for the index first, then suggestions:

```sh
es PUT /pages/_settings '{
  "index.roll.analytics.enabled": true,
  "index.roll.suggestions.enabled": true,
  "index.roll.suggestions.min_count": 5
}'
```

The CLI does the same: `roll search indices settings set docs pages index.roll.analytics.enabled=true
index.roll.suggestions.enabled=true index.roll.suggestions.min_count=5`.

When your backend searches for someone, send their text in the `X-Roll-Search-Query` header so
analytics records exactly what they typed, not your query's internals:

```sh
curl "$ELASTICSEARCH_URL/pages/_search" -H "Authorization: ApiKey $ELASTICSEARCH_API_KEY" \
  -H 'Content-Type: application/json' -H 'X-Roll-Search-Query: kubernetes install' \
  -d '{"query": {"match": {"body": "kubernetes install"}}}'
```

A query becomes a suggestion once at least `min_count` searches used it in the window (30 days by
default), so one person's unusual search is never shown to others. roll rebuilds the suggestions
once a day into the index behind the alias `pages-suggestions`. To build them now, use **Rebuild now**
in the **Relevance** tab's **Autocomplete** section. Query them with the completion suggester:

```sh
es POST /pages-suggestions/_search '{
  "_source": false,
  "suggest": {"q": {"prefix": "kub", "completion": {"field": "suggest", "skip_duplicates": true, "size": 5}}}
}'
```

The **Relevance** tab also shows top queries and queries that found nothing: good candidates for
new synonyms. The settings, weights and privacy rules are in [../README.md](../README.md), "Search
analytics" and "Query suggestions".

## Search from the browser

A web page can't call the cluster directly yet: clusters don't send CORS headers. Browser keys
(search-only keys a page can ship, with allowed origins and rate limits) are coming with #358,
which is in review and not on main.

Until then, put a small endpoint in front of search. Give it a key that can only read `pages` and
`pages-suggestions`:

```sh
roll search keys create docs site-search-backend --read-only --indices pages,pages-suggestions
```

Use the printed `api_key` as the endpoint's key. It names the cluster, so it works on any of its
addresses. The key can search and read those two indices and nothing else: no writes, no other
index, no cluster settings.

```js
// server.mjs: SEARCH_KEY=… node server.mjs, then GET http://localhost:3000/search?q=…
import http from 'node:http'
import { Client } from '@elastic/elasticsearch'

const es = new Client({ node: process.env.ELASTICSEARCH_URL, auth: { apiKey: process.env.SEARCH_KEY } })

http.createServer(async (req, res) => {
  const q = (new URL(req.url, 'http://localhost').searchParams.get('q') ?? '').slice(0, 200)
  try {
    const r = await es.search({
      index: 'pages',
      size: 8,
      _source: ['url', 'title'],
      query: { multi_match: { query: q, type: 'bool_prefix', fuzziness: 'AUTO',
        fields: ['title', 'title._2gram', 'title._3gram', 'body'] } },
      highlight: { fields: { body: { number_of_fragments: 1 } } },
    }, { headers: { 'x-roll-search-query': q } })
    res.writeHead(200, { 'content-type': 'application/json', 'access-control-allow-origin': 'https://example.com' })
    res.end(JSON.stringify(r.hits.hits))
  } catch (e) {
    res.writeHead(502).end()
  }
}).listen(3000)
```

Replace `https://example.com` with your site's origin.
