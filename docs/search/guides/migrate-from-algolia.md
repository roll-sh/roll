# Migrate from Algolia, Typesense or Meilisearch

roll copies an Algolia index, a Typesense collection or a Meilisearch index into a roll search
cluster in one step: the records, and the settings, synonyms and rules mapped onto what
Elasticsearch has. roll's servers read the source. Your browser and your laptop never connect to
it, and the key you give is used for that migration only. roll never stores it, returns it, logs
it, or puts it in a task's description.

Mapping one search engine onto another is never exact. roll maps what has an equivalent, and
lists everything that doesn't with the reason. It never drops a setting, a synonym or a rule
without saying so, and you see that list before anything is made.

Moving from Elasticsearch or OpenSearch is the same flow with **Elasticsearch** or **OpenSearch**
picked as the source: it copies indices as they are, by reindex from remote
([../README.md](../README.md), "Migrating into roll").

## Run a migration

### In the dashboard

1. Open your cluster and click **Migrate**.
2. Pick the source: **Algolia**, **Typesense** or **Meilisearch**.
3. Fill in where it is and the key:
   - Algolia: the **Application ID** and an API key. Use the Admin API key, or a key with the
     `listIndexes`, `browse` and `settings` ACLs (synonyms and rules are read with `settings`).
   - Typesense: the URL (Typesense Cloud's `https://xxx.a1.typesense.net`, or your own server's)
     and an admin key, or one that allows `collections:get`, `documents:export`, `synonyms:get`
     and `overrides:get`.
   - Meilisearch: the URL and the master key, or a key with `indexes.get`, `documents.get`,
     `settings.get` and `stats.get`. A server without a master key takes no key.
4. Click **List indices**, pick the indices to copy and, if you want, a new name for each copy.
   An Algolia replica shows as "replica of …": copy its primary, and the replica comes along as
   a sort (see [Replicas](#replicas)).
5. Click **Review mapping**. roll reads each index's definition and shows what it becomes:
   the field used as `_id`, the synonyms and rules it brings, the search template it makes,
   and everything that didn't map, grouped by settings, fields, replicas, synonyms and rules.
   Nothing is made yet.
6. Click **Migrate**. Each index is created with its mappings, synonyms set, query rulesets and
   search template, and its documents are copied by a task. The panel shows each copy's
   progress, and you can cancel one. Closing the panel doesn't stop the copies.
7. When a copy is done, point an alias at it so clients keep using the old name.

### From the CLI

The key comes from the environment (or stdin with `--api-key-stdin`), never from the command
line:

```sh
export ALGOLIA_API_KEY=...          # TYPESENSE_API_KEY, MEILISEARCH_API_KEY
roll search migrate shop --from algolia --app-id YourAppID                  # list the indices
roll search migrate shop --from algolia --app-id YourAppID --index products --dry-run
roll search migrate shop --from algolia --app-id YourAppID --index products=catalog --wait
roll search migrate shop --from typesense --host https://xxx.a1.typesense.net --index books --wait
roll search migrate shop --from meilisearch --host https://ms.example.com --index movies --wait
```

`--dry-run` prints what each index becomes and every line that didn't map. `--wait` follows the
copies to their end, and starts copies that had to wait for a free slot.

### Through the API

`POST /v1/search/clusters/{id}/migrate/inspect` and `POST /v1/search/clusters/{id}/migrate`
take a `source` of `{"kind": "algolia", "app_id": "...", "api_key": "..."}`,
`{"kind": "typesense", "host": "https://...", "api_key": "..."}` or
`{"kind": "meilisearch", "host": "https://...", "api_key": "..."}`. Add `"dry_run": true` to
`migrate` to get the plan without making anything. The contract is in
[../README.md](../README.md) (the `/v1/search` table).

## Search the copy as the source did

Each copy gets a search template, `<index>-search`, that searches the way the source did as
closely as Elasticsearch can say it: the source's searchable attributes with their weights,
typo tolerance as `fuzziness`, the last word as a prefix (search as you type), the query rules,
the custom ranking as a sort after relevance, and `distinct` as a field collapse. Pass the query
text as `query`:

```sh
curl -u shop:$PASSWORD "$URL/products/_search/template" -H 'Content-Type: application/json' \
  -d '{"id": "products-search", "params": {"query": "red shoes"}}'
```

A rule with an Algolia context matches only when the search passes it: add
`"context": "mobile"` to `params`.

### Query rules match the text as sent

Elasticsearch's query rules compare the search text exactly, byte for byte, and roll keeps
Elasticsearch's behaviour. Algolia, Typesense and Meilisearch normalize the query first, so the
same rule behaves differently in two ways:

- **Case.** The rules' patterns are stored lowercased, so a pin on `iphone` doesn't fire for
  "iPhone" unless the text reaches the rule lowercased.
- **`contains` is a substring test.** A rule on `shoe` with `contains` fires for "horseshoes"
  too. Algolia matches whole words.

The template matches rules against `rules_query` when you pass it, and against `query`
otherwise. Mustache can't lowercase, so send the lowercased text yourself:

```js
// InstantSearch through Searchkit, or any client: one extra parameter.
const params = { query: q, rules_query: q.toLowerCase().trim() };
await client.searchTemplate({ index: "products", id: "products-search", params });
```

The review lists every mapped rule in a note that says this.

### Filter the results

Pass one Elasticsearch query as `filters` to filter what the template returns (facet filters,
ranges, anything a `bool` filter takes):

```json
{"id": "products-search", "params": {"query": "kettle", "filters": {"term": {"brand.keyword": "Acme"}}}}
```

Pins follow Algolia's rule: a rule's promoted records show whatever the filters say, unless the
rule set `filterPromotes`, whose pins show only when they match the filters too (they're kept in
their own rulesets, `<index>-rules-filtered`).

### Rules that filter or rewrite the query

Elasticsearch's query rules only pin and exclude documents, and a Mustache template can't compare
the query with a pattern. So an Algolia rule whose consequence sets `params.filters`,
`facetFilters`, `numericFilters` or a `query` (a new query, or edits) becomes a search template
of its own, `<index>-search-rule-<rule>`: the default template with the rule's filter added and
its query used. The review, and the index's `_meta.roll_import.rule_templates`, list each with
its condition (`anchoring` and `pattern`). Your client picks the template when the query meets
the condition:

```js
const rules = (await client.indices.getMapping({ index: "products" })).products.mappings._meta.roll_import.rule_templates;
const hit = rules.find((r) => (r.anchoring === "is" ? q === r.pattern : r.anchoring === "contains" ? q.includes(r.pattern) : false));
await client.searchTemplate({ index: "products", id: hit?.template ?? "products-search", params: { query: q } });
```

Query edits (`remove`, `replace`) are applied to the pattern when the rule's anchoring is `is`,
where the query's text is known; on other anchorings they're listed. Algolia's filter syntax
maps to Elasticsearch queries: `attr:value` and `attr:"a value"` (on the exact field), `attr <
10` and the other comparisons, `attr:10 TO 20`, `AND`, `OR`, `NOT` and parentheses; facet and
numeric filter lists (inner lists are ORs, a leading `-` negates). Filter scores and optional
filters are listed.

Plain searches work too. The index's default fields (`index.query.default_field`) are the
source's searchable attributes, so a `multi_match` or `query_string` without `fields` searches
them with the same weights. The synonyms apply to every search, through the index's search
analyzer.

## What maps to what

### Algolia

| Algolia | On roll |
| --- | --- |
| Records (`browse`, page by page) | Documents. `objectID` is `_id` and stays in the document as a `keyword` field. `_highlightResult` and the other decorations are left out. |
| `searchableAttributes` | Text fields, and the index's default fields with weights in Algolia's order (the first attribute weighs the most; `a,b` share a weight). `unordered()` is the default on roll. An attribute that holds objects searches its fields (`attr.*`). |
| `attributesForFaceting` | A `keyword` sub-field (`brand.keyword`) on a text field, or a `keyword` field when the attribute isn't searchable. Aggregate on it for facets, filter with `term`. `searchable()` and `filterOnly()` need nothing more. |
| `customRanking` | A sort after relevance (`_score`, then `popularity desc`) in the search template. |
| `ranking` | `asc(attr)` / `desc(attr)` before the textual criteria become a sort before relevance. |
| `typoTolerance`, `minWordSizefor1Typo`, `minWordSizefor2Typos` | `fuzziness: AUTO:4,8` (Algolia's word sizes) in the template; off when typo tolerance is off. |
| `queryType: prefixLast` (the default) | `multi_match` of type `bool_prefix`: the last word matches as a prefix. `prefixNone` turns it off. |
| `ignorePlurals` | A plural-only stemmer (`minimal_english`, `minimal_french`, `minimal_german`, `minimal_portuguese`, or the light stemmer of Spanish, Italian, Swedish, Finnish, Russian, Norwegian; `dutch`) for the first language. |
| `removeStopWords` | Elasticsearch's stop word list for each language (`_english_` …), at search time, after the synonyms: a synonym that holds a stop word (`it, information technology`) still works. |
| Accents and case | Folded at index and search time (`asciifolding`, `lowercase`), as Algolia does. |
| `unretrievableAttributes` | `_source.excludes`: still searchable, never returned. Excluded fields can't be copied again from roll. |
| `attributesToRetrieve` | `_source` in the template. |
| `hitsPerPage` | `size` in the template. |
| `distinct`, `attributeForDistinct` | A field collapse in the template (`inner_hits` keeps the first N of a group when `distinct` is above 1). |
| Synonyms: `synonym` | `a, b, c` in the synonyms set `<index>-synonyms`. |
| `onewaysynonym` | `input => input, s1, s2` (the input still matches itself). |
| `altcorrection1`, `altcorrection2` | One-way synonyms. Their corrections aren't ranked as typos (noted). |
| Rules: conditions | Query rule criteria on the query text, lowercased: `is` is `exact`, `startsWith` is `prefix`, `endsWith` is `suffix`, `contains` is `contains`, no pattern is `always`. A `context` is a criterion on the search's `context`. Matching is case-sensitive (see [above](#query-rules-match-the-text-as-sent)). |
| Rules: `promote` | A `pinned` rule. Pinned documents go to the top in their order (positions aren't kept, noted). |
| Rules: `hide` | An `exclude` rule (`<rule>-exclude`). |
| Rules: `filterPromotes` | The rule's pins in `<index>-rules-filtered`, which apply inside the template's `filters`. |
| Rules: `params.filters`, `facetFilters`, `numericFilters`, `query` | A search template of the rule's own (see [above](#rules-that-filter-or-rewrite-the-query)). Other parameters are listed. |
| Replicas | Sorts on the one index, each a search template `<index>-search-<replica>` (see below). |

#### Replicas

Algolia keeps a copy of the index per sort order. Elasticsearch sorts at search time, so roll
copies the primary once and turns each replica into a search template with the replica's sort:

- A standard replica's `ranking` led by `asc(price)` sorts by `price` first, then relevance.
- A virtual replica's `customRanking` sorts by relevance, then its custom ranking.

`products_price_asc` becomes the template `products-search-products_price_asc`. A replica that
sorts no differently from its primary is listed as not mapped.

### Typesense

| Typesense | On roll |
| --- | --- |
| Documents (`documents/export`, JSONL, read as it streams) | Documents. `id` is `_id` and stays a `keyword` field. |
| `string`, `string[]` | `text`, with a `keyword` sub-field when `facet` or `sort` is on. `index: false` is a `keyword` that isn't indexed. |
| `int32`, `int64`, `float`, `bool` (and arrays) | `integer`, `long`, `float`, `boolean`. |
| `float[]` with `num_dim` | `dense_vector` (`cosine`, or `max_inner_product` for `vec_dist: ip`). The vectors are copied; an `embed` model isn't (noted). |
| `geopoint` | `geo_point`. Typesense's `[lat, lng]` is written as `{"lat": …, "lon": …}`, because Elasticsearch's array form is `[lon, lat]`. |
| `object`, `object[]`, `auto`, `string*` | Dynamic mapping. |
| `store: false` | `_source.excludes`. |
| `default_sorting_field` | A sort after relevance in the template. |
| Searched fields | Typesense names them per search (`query_by`). The index's default fields are every indexed string field. |
| Typos and prefixes | `fuzziness: AUTO:4,7` and `bool_prefix`, Typesense's defaults. |
| Synonyms (multi-way) | `a, b, c`. |
| Synonyms with a `root` | `root => root, s1, s2`. |
| Overrides: `rule.query` with `match` | Criteria `exact` or `contains` on the query text. |
| Overrides: `includes` | A `pinned` rule (in position order). |
| Overrides: `excludes` | An `exclude` rule. |
| Synonym sets (Typesense 29: `synonym_sets` in the schema, `/synonym_sets/<name>`) | Read with the collection's own synonyms, mapped the same way. |
| Curation sets (Typesense 30: `curation_sets` in the schema, `/curation_sets/<name>`) | Read with the collection's own overrides, mapped the same way. |

### Meilisearch

| Meilisearch | On roll |
| --- | --- |
| Documents (`documents?offset&limit`, page by page) | Documents. The `primaryKey` is `_id`. |
| `searchableAttributes` | Text fields and default fields with weights in order (`["*"]`: every field, Elasticsearch's default). |
| `filterableAttributes`, `sortableAttributes` | A `keyword` sub-field on text, or a `keyword` field. Numbers stay numbers. |
| `rankingRules`: `attr:asc`, `attr:desc` | A sort after relevance in the template. |
| `typoTolerance` | `fuzziness: AUTO:5,9` (its `minWordSizeForTypos`); off when disabled. |
| `prefixSearch` | `bool_prefix` unless it's `disabled`. |
| `stopWords` | A `stop` filter at search time. |
| `synonyms` | `word => word, s1, s2` for each entry (Meilisearch's are one-way). |
| `distinctAttribute` | A field collapse in the template. |
| `displayedAttributes` | `_source` in the template. |

## What doesn't map

Each of these is listed in the review, with the reason, when the source sets it:

- **Ranking criteria.** Algolia's tie-breaking criteria (`typo`, `geo`, `words`, `filters`,
  `proximity`, `attribute`, `exact`) and Meilisearch's ranking rules (`words`, `typo`,
  `proximity`, `attribute`, `exactness`) have no Elasticsearch equivalent. roll ranks by BM25
  relevance, then by the custom ranking as a sort. Expect the order of results to differ
  somewhat: try the queries that matter on a [branch](../README.md) and tune the template.
- **Algolia rule consequences roll can't say**: search parameters other than filters and the
  query (`aroundLatLng`, `hitsPerPage` …), query edits on a pattern that isn't `is`, filter
  scores, `userData` (no query rule returns data: keep it in your client), validity periods, and
  conditions with a `{facet:…}` placeholder or on filters. A rule keeps whatever else it does,
  and the rest is listed.
- **Typesense overrides that filter, sort or rewrite** (`filter_by`, `sort_by`,
  `replace_query`, dynamic `{placeholder}` queries, `tags`, effective dates, `metadata`).
- **Algolia placeholder synonyms**, which match a token in records.
- **Typo settings per attribute or word** (`disableTypoToleranceOnAttributes`,
  `disableTypoToleranceOnWords`, Meilisearch's `disableOnWords` and `disableOnAttributes`),
  and `typoTolerance: min` or `strict`: fuzziness applies to every word and field the template
  searches.
- **Language-specific settings**: `decompoundedAttributes`, `camelCaseAttributes`,
  Typesense's `locale`, `stem`, `infix`, `token_separators` and `symbols_to_index`,
  Meilisearch's `dictionary`, `separatorTokens`, `nonSeparatorTokens` and
  `localizedAttributes`. The standard tokenizer splits words its own way.
- **Typesense image fields** and **Meilisearch embedders**: vectors that are in the documents
  are copied; the models that make them aren't. Add an inference endpoint and a `semantic_text`
  field on roll ([rag.md](rag.md)).
- **Anything else** a source sets that changes what matches or how it ranks, by name. Display
  settings (highlight tags, snippets, facet value counts) aren't listed: they're the client's.

- **Attribute names Elasticsearch can't take**: its metadata fields (`_id`, `_source`,
  `_routing` …) are left out of the mappings and out of the copied documents, and a name the
  search template can't carry (one with `{`, `}`, `"`, a space, an empty `asc()`) is left out of
  the template's fields, sorts and `_source`.

Limits, each listed when it's reached:

- 10,000 synonyms per index (one synonyms set).
- 1,000 query rules per index: one search applies 10 rulesets of 100. A source rule makes one
  query rule for its promotions and one for its hides, for each of its conditions, so 600
  Algolia rules that both promote and hide make 1,200, and the last 200 are listed.
- 100 documents per pinned or excluded rule.
- The sorts of 20 Algolia replicas.

## How the copy runs

- Each copy is a task in the cluster's `_tasks` (`indices:data/write/reindex`), like a reindex
  from remote. `GET _tasks/<id>` shows its progress, and `POST _tasks/<id>/_cancel` stops it.
- It reads one page at a time (Algolia's `browse` cursor, Meilisearch's offset pages, the
  Typesense export line by line) and writes it with `_bulk` before reading the next, so a
  copy of any size holds one page in memory. Pages are charged to the node's reindex budget.
  A page larger than the node's answer limit (16 MiB unless the operator sets
  `roll.reindex.remote.buffer_limit`) is refused rather than read: lower the batch size.
- A cluster runs at most 2 copies at once (and its owner and the node have limits too). Copies
  past them wait, and the dashboard and `--wait` start them as others end.
- A copy stops at the first page roll can't write, as Elasticsearch's reindex does, and when
  the source fails a page or the copy is cancelled. It saves its place in the index's
  `_meta.roll_import` (every 5 seconds while it runs, and when it stops), never a key. Pick the
  same index of the same source (application or URL) with the same copy name, and the copy
  resumes there (`full: true`, `--full` or a fresh name copies everything again):
  - Algolia from its `browse` cursor after the last page written;
  - Meilisearch from its offset;
  - Typesense from the line after the last one written. The export has no cursor, so the lines
    before it are read again and passed over, not written.
- A source that sends a cursor it sent before, or more than twice the documents it said it
  holds, is stopped.
- The search analyzer runs case and accent folding, then the synonyms, then stop words and
  plurals; the review builds it, so a synonym it can't parse shows up there, before anything is
  made.
- A copy is one point in time: writes made to the source while it runs may be missed. Catch up
  before you switch (below).

## Catch up and cut over

Once the copy is done, keep the source in use while you check roll, then move over:

1. **Catch up** as often as you like (`"step": "catch_up"`, `--catch-up`, the row's **Catch up**
   in the dashboard). It reads the source again from the time the copy (or the last catch-up)
   started, less 5 minutes for clocks that differ, compares each page with what roll holds, and
   writes only the documents that differ. The task counts them as created, updated, and unchanged
   (`noops`).
2. **Stop writing to the source**, then catch up one last time.
3. **Cut over** (`"step": "cutover"`, `--cutover`, **Cut over**): an alias, the source's index
   name unless you name another, moves to the copy in one step, from whatever index held it.
   Clients that search the alias now search the copy. No key is needed: roll doesn't call the
   source.

How much a catch-up reads depends on what the source lets roll ask:

| Source | With `since_attribute` (a numeric attribute your records set when they change) | Without |
| --- | --- | --- |
| Algolia | `browse` with `numericFilters: ["<attr> >= <time>"]`: only changed records | Every record, compared |
| Typesense | the export with `filter_by=<attr>:>=<time>` | Every record, compared |
| Meilisearch | documents with `filter=<attr> >= <time>` (the attribute must be filterable) | Every record, compared |

`since_unit` says whether the attribute counts seconds (`s`, the default) or milliseconds
(`ms`) since the epoch.

What no catch-up can do:

- **Deletes.** None of the three tells roll what was deleted, so records deleted at the source
  stay on roll. Delete them on roll yourself, or copy again into a fresh index.
- **Records changed without touching the attribute.** With `since_attribute`, a record whose
  attribute wasn't updated isn't read. Without one, every record is read, and changes are found
  by comparing.
- **Writes after the last catch-up.** Only a catch-up after the source stops taking writes
  leaves nothing behind.

## Security

- Every call to the source goes through roll's guarded client: public addresses only, no
  redirects, bounded answers and timeouts, under the operator's deny lists. Algolia's host is
  fixed by the application id (`https://<app>-dsn.algolia.net`); a Typesense or Meilisearch URL
  is checked like any URL you give roll.
- The key is sent in the request body, used for that request and the copies it starts, and held
  in memory only while they run. An error never quotes what the source answered (its status and
  error type at most), so a source can't echo the key back to you through roll.
- `migrate` (and so the review, its `dry_run`) needs `admin` on the cluster in a scoped token,
  `inspect` needs `write` ([../README.md](../README.md)). The dashboard says so when it's
  refused.
- A Typesense or Meilisearch URL over plain `http://` sends the key unencrypted: the
  dashboard warns before you list.
