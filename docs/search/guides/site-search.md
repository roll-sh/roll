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

A page can call the cluster itself, with no backend of its own in front, using a browser key. A
browser key can only search, reaches only what you name, and is rate limited, so it's safe to ship
in the page's JavaScript.

In the dashboard, open the cluster's **Settings**, then **Browser search**:

1. Under **Allowed origins**, add your site's origin, one per line: `https://docs.example.com`,
   plus `http://localhost:3000` while you develop. Click **Save**.
2. Under **Browser keys**, name the key `site`, set **Reaches** to **Indices** with
   `pages, pages-suggestions`, and set **Per address** to `60` so one visitor can't spend the
   key's whole budget. Create the key and copy it, because it's shown only once.

Browsers have to call the cluster at its own URL (`https://docs.search.…`), since that's how a
preflight request, which carries no key, names its cluster. A shared address such as
`http://127.0.0.1:59200` can't serve browsers.

```html
<input id="q" type="search" placeholder="Search the docs" autocomplete="off">
<ul id="hits"></ul>
<script type="module">
const CLUSTER = 'https://docs.search.…'   // the cluster's URL
const KEY = '…'                            // the browser key
const box = document.getElementById('q'), list = document.getElementById('hits')
let inflight
box.addEventListener('input', async () => {
  inflight?.abort()
  inflight = new AbortController()
  const q = box.value.slice(0, 200)
  if (!q.trim()) { list.replaceChildren(); return }
  try {
    const r = await fetch(`${CLUSTER}/pages/_search`, {
      method: 'POST',
      signal: inflight.signal,
      headers: { authorization: `ApiKey ${KEY}`, 'content-type': 'application/json' },
      body: JSON.stringify({
        size: 8,
        _source: ['url', 'title'],
        query: { multi_match: { query: q, type: 'bool_prefix', fuzziness: 'AUTO',
          fields: ['title', 'title._2gram', 'title._3gram', 'body'] } },
        highlight: { fields: { body: { number_of_fragments: 1 } } },
      }),
    })
    if (!r.ok) return   // 429: the key's rate limit; the next keystroke tries again
    const { hits } = await r.json()
    list.replaceChildren(...hits.hits.map((h) => {
      const li = document.createElement('li'), a = document.createElement('a')
      a.href = h._source.url
      a.textContent = h._source.title
      li.append(a)
      return li
    }))
  } catch (e) {
    if (e.name !== 'AbortError') throw e
  }
})
</script>
```

The page sends no `X-Roll-Search-Query` header. Analytics reads the visitor's text from the
`multi_match` query, so searches still count toward the **Relevance** tab and query suggestions.
To keep some pages out of results, give the key a **Filter**, such as
`{"term": {"public": true}}`: every search the key makes is filtered by it, whatever the page
sends. The panel's **Search UI** and **InstantSearch (Searchkit)** snippets set up those libraries
with the same key.

A browser key can't index, change settings or read other indices. It's still public, though:
anyone can copy it out of the page and search `pages` from a script, within its rate limits.
Allowed origins only bind browsers. The limits a browser key works under are listed in
[../README.md](../README.md), "Search from the browser".

### A key per end customer

If your site serves many customers from one cluster (a SaaS whose customers each search their own
documents), don't make a browser key for each. Make one *derivable* key, and let your backend sign
a short-lived, narrower key for each visitor, with no call to roll, as Algolia's secured API keys
work:

1. Under **Browser keys**, create a key that reaches `pages` and check **Derivable**. Copy its
   key id from the list, and its **signing secret**, which is shown only once. The secret stays
   on your backend: it signs any key the parent allows.
2. When your backend renders a page for a customer, it signs a key for that customer and puts it
   in the page in place of the browser key. A derived key can only narrow its parent: some of its
   indices, a filter ANDed with the parent's, an expiry at most 24 hours ahead, and a `user` that
   gets its own share of the parent's rate limit, so one customer can't use it up.

```js
// derive.mjs (Node 18+): a key for one customer, signed on your backend
import { createHmac } from 'node:crypto'

export function deriveKey(secret, params) {
  const signed = `rdk1.${Buffer.from(JSON.stringify(params)).toString('base64url')}`
  return `${signed}.${createHmac('sha256', secret).update(signed).digest('base64url')}`
}

const key = deriveKey(process.env.SIGNING_SECRET, {
  key: 'Jx2kq3Z0RkKp1M7m0nQv',               // the derivable browser key's id
  cluster: 'docs',                            // the cluster's name
  exp: Math.floor(Date.now() / 1000) + 3600,  // an hour from now (24 hours at most)
  filter: { term: { tenant: 'acme' } },       // ANDed with the parent's filter
  user: 'acme',                               // its own share of the parent's rate limit
})
```

```python
# derive.py: the same in Python
import base64, hashlib, hmac, json, time

def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b'=').decode()

def derive_key(secret, params):
    signed = 'rdk1.' + b64(json.dumps(params, separators=(',', ':')).encode())
    return signed + '.' + b64(hmac.new(secret.encode(), signed.encode(), hashlib.sha256).digest())

key = derive_key(SIGNING_SECRET, {'key': 'Jx2kq3Z0RkKp1M7m0nQv', 'cluster': 'docs',
                                  'exp': int(time.time()) + 3600,
                                  'filter': {'term': {'tenant': 'acme'}}, 'user': 'acme'})
```

```go
// derive.go: the same in Go, standard library only
func DeriveKey(secret string, params map[string]any) (string, error) {
	payload, err := json.Marshal(params)
	if err != nil {
		return "", err
	}
	signed := "rdk1." + base64.RawURLEncoding.EncodeToString(payload)
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(signed))
	return signed + "." + base64.RawURLEncoding.EncodeToString(mac.Sum(nil)), nil
}
```

The page sends the derived key exactly as it sent the browser key (`Authorization: ApiKey …`),
so the snippet above, Search UI and InstantSearch need no change. Other fields you can set:
`indices` (some of the parent's, or patterns within its patterns), `origins` (pages it may be
sent from, among the cluster's allowed origins) and `rate_limit: {"per_minute": n}` (the user's,
below its share). A filter may only test the document itself (`term`, `terms` of values,
`range`, `exists`, the `match` queries, in `bool`): no terms lookups, joins or scripts. The
cluster refuses a key that would widen its parent with a 401 that names the field; a key with a
wrong signature, or for a parent that isn't derivable, gets a plain `derived key refused`. To check your own signer, sign the `payload` of
`crates/search-es/tests/fixtures/derived-key-vector.json` with its `secret`: you get its `key`.
To try one from a terminal:

```sh
ROLL_SEARCH_SIGNING_SECRET=rdks_… roll search browser-keys derive docs Jx2kq3Z0RkKp1M7m0nQv \
  --expires-in 1h --filter '{"term":{"tenant":"acme"}}' --user acme
```

Revoking the parent stops every key derived from it at once. If the secret leaks, rotate it
(**Rotate secret** in the list): keys signed with the old one stop working with their next
request, and your backend signs with the new one from then on.

### On a preview

A pull request's preview (roll previews) searches a branch of the cluster, not the cluster
itself, and the browser key the site ships is the production cluster's: it doesn't work on the
branch. roll previews makes the branch browser keys of its own, one for each of production's
(the same indices, filter and limits; a derivable one gets a signing secret of the branch's own),
and lets the preview's pages call the branch: its allowed origins are production's plus the
preview's own URLs. The preview's services get them as `ROLL_SEARCH_BROWSER_KEYS` (JSON, one
entry per key: `name`, `id`, `api_key` and, for a derivable one, `signing_secret`), and when
production has exactly one browser key, as `ROLL_SEARCH_BROWSER_KEY`,
`ROLL_SEARCH_BROWSER_KEY_ID` and `ROLL_SEARCH_SIGNING_SECRET`, beside `ELASTICSEARCH_URL`.
Have your build take the key from there when it's set, and the production key otherwise:

```js
// The key the page ships: the preview's own when roll previews set one.
const KEY = process.env.ROLL_SEARCH_BROWSER_KEY ?? process.env.NEXT_PUBLIC_SEARCH_KEY
const CLUSTER = process.env.ELASTICSEARCH_URL ?? 'https://docs.search.…'
```

A backend that derives keys does the same with `ROLL_SEARCH_BROWSER_KEY_ID` and
`ROLL_SEARCH_SIGNING_SECRET`, and names the branch's cluster in what it signs. The keys are
revoked when the preview's branch goes.

### Or keep a backend

If you'd rather not ship a key, put a small endpoint in front of search. Give it a key that can
only read `pages` and `pages-suggestions`:

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
