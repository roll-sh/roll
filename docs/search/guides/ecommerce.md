# E-commerce search

Product search for a shop: a mapping that serves both full-text search and filters, facets with
counts, sorting, popular products first, merchandising rules that pin or hide products, and click
analytics. About fifteen minutes.

## Before you start

You need a cluster, its URL and a key, as in [the first query](README.md#the-first-query-in-a-minute).
Query rules and analytics collections need an admin key, so use one for setup:

```sh
roll search clusters create shop
roll search keys create shop setup --access admin --expires-in 1d
export ELASTICSEARCH_URL=…        # printed by the commands above
export ELASTICSEARCH_API_KEY=…
```

The requests use the `es` helper from [the guides' README](README.md#a-shorter-way-to-send-requests).

## Map your products

```sh
es PUT /products '{
  "mappings": {
    "properties": {
      "sku":         {"type": "keyword"},
      "name":        {"type": "text", "fields": {"keyword": {"type": "keyword"}}},
      "description": {"type": "text"},
      "brand":       {"type": "keyword"},
      "category":    {"type": "keyword"},
      "color":       {"type": "keyword"},
      "price":       {"type": "scaled_float", "scaling_factor": 100},
      "in_stock":    {"type": "boolean"},
      "popularity":  {"type": "integer"},
      "created_at":  {"type": "date"}
    }
  }
}'
```

- `text` fields are split into words for full-text search. `keyword` fields are kept whole, for
  exact filters, facets and sorting. Brand, category and color are filters, so they're `keyword`.
- `name` is both: `name` for search, `name.keyword` (a multi-field) for sorting by name or an
  exact match.
- `scaled_float` with a factor of 100 stores prices as whole cents.

## Load products

Save this as `products.ndjson` (each document follows its action line, and the file ends with a
newline):

```json
{"index": {"_id": "sku-1001"}}
{"sku": "sku-1001", "name": "Trail Runner 3", "description": "Light trail running shoe", "brand": "Northpeak", "category": "shoes", "color": "red", "price": 89.99, "in_stock": true, "popularity": 940, "created_at": "2026-09-01"}
{"index": {"_id": "sku-1002"}}
{"sku": "sku-1002", "name": "Road Runner", "description": "Cushioned road running shoe", "brand": "Swiftline", "category": "shoes", "color": "blue", "price": 119.00, "in_stock": true, "popularity": 610, "created_at": "2026-06-12"}
{"index": {"_id": "sku-1003"}}
{"sku": "sku-1003", "name": "Trail Runner 2", "description": "Last season's trail shoe", "brand": "Northpeak", "category": "shoes", "color": "red", "price": 59.00, "in_stock": false, "popularity": 1200, "created_at": "2025-08-20"}
{"index": {"_id": "sku-2001"}}
{"sku": "sku-2001", "name": "Running Socks", "description": "Merino running socks, two pairs", "brand": "Swiftline", "category": "socks", "color": "black", "price": 18.50, "in_stock": true, "popularity": 300, "created_at": "2026-02-02"}
```

```sh
es POST '/products/_bulk?refresh=true' @products.ndjson
```

For a real catalogue, send batches of a few thousand documents (or a few MB) per `_bulk` call,
and check the answer's `errors` flag. The dashboard's **Indices** tab can also import an NDJSON,
JSON or CSV file into an index.

## Search with typo tolerance

```sh
es POST /products/_search '{
  "query": {
    "multi_match": {
      "query": "trail runer",
      "fields": ["name^3", "brand^2", "description"],
      "fuzziness": "AUTO",
      "operator": "and"
    }
  }
}'
```

`runer` still finds "Runner": with `fuzziness: AUTO`, words of 3–5 characters may be one edit
off, longer words two. `name^3` makes a match in the name count three times as much as one in the
description. `operator: and` requires every word, which suits short product queries.

## Add facets

A facet is a list of values with counts next to the results ("Brand: Northpeak (2), Swiftline
(2)"). Ask for them with aggregations on `keyword` and numeric fields:

```sh
es POST /products/_search '{
  "size": 10,
  "query": {"multi_match": {"query": "running", "fields": ["name^3", "description"], "fuzziness": "AUTO"}},
  "aggs": {
    "brands":     {"terms": {"field": "brand", "size": 20}},
    "categories": {"terms": {"field": "category", "size": 20}},
    "colors":     {"terms": {"field": "color", "size": 20}},
    "price": {"range": {"field": "price", "ranges": [{"to": 50}, {"from": 50, "to": 100}, {"from": 100}]}}
  }
}'
```

## Filter by a facet and keep its other values

When the shopper picks "red", the results should show only red products, but the color facet
should still list the other colors so they can switch. Put the color filter in `post_filter`: it
narrows the hits after the aggregations ran. Filter the *other* facets by color yourself:

```sh
es POST /products/_search '{
  "query": {
    "bool": {
      "must": {"multi_match": {"query": "running", "fields": ["name^3", "description"], "fuzziness": "AUTO"}},
      "filter": [{"term": {"category": "shoes"}}]
    }
  },
  "post_filter": {"term": {"color": "red"}},
  "aggs": {
    "colors": {"terms": {"field": "color"}},
    "brands": {"filter": {"term": {"color": "red"}}, "aggs": {"values": {"terms": {"field": "brand"}}}}
  }
}'
```

Filters that should narrow everything, facets included (the category the shopper is browsing),
go in `bool.filter`. Filters in `filter` don't affect scores and are cached.

## Sort results

```sh
es POST /products/_search '{
  "query": {"bool": {"filter": [{"term": {"category": "shoes"}}]}},
  "sort": [{"price": "asc"}, {"name.keyword": "asc"}]
}'
```

Other common orders: `{"created_at": "desc"}` for newest first, `{"popularity": "desc"}` for best
sellers. A sort on a `text` field fails; sort on its `keyword` multi-field.

## Show popular products first

Relevance alone ranks a product nobody buys next to a best seller. `function_score` mixes in your
own signals:

```sh
es POST /products/_search '{
  "query": {
    "function_score": {
      "query": {"multi_match": {"query": "trail shoe", "fields": ["name^3", "description"], "fuzziness": "AUTO"}},
      "functions": [
        {"field_value_factor": {"field": "popularity", "modifier": "log2p", "missing": 0}},
        {"filter": {"term": {"in_stock": true}}, "weight": 2}
      ],
      "score_mode": "multiply",
      "boost_mode": "multiply"
    }
  }
}'
```

`log2p` takes `log10(popularity + 2)`, so a product sold 1,000 times ranks above one sold 10
times without burying every new product. In-stock products count double. Compare two versions of
a query side by side in the dashboard's **Playground** tab, with each hit's score explained.

## Pin and hide products

Query rules let merchandisers change results for given searches without touching code: pin a
product to the top for "running shoes", or hide a recalled one everywhere.

```sh
es PUT /_query_rules/merch '{
  "rules": [
    {
      "rule_id": "pin-trail-runner",
      "type": "pinned",
      "criteria": [{"type": "exact", "metadata": "user_query", "values": ["running shoes", "trail shoes"]}],
      "actions": {"ids": ["sku-1001"]}
    },
    {
      "rule_id": "hide-recalled",
      "type": "exclude",
      "criteria": [{"type": "always"}],
      "actions": {"ids": ["sku-1003"]}
    }
  ]
}'
```

Wrap your query in a `rule` query and pass what the shopper typed as `user_query`:

```sh
es POST /products/_search '{
  "query": {
    "rule": {
      "organic": {"multi_match": {"query": "running shoes", "fields": ["name^3", "description"], "fuzziness": "AUTO"}},
      "ruleset_ids": ["merch"],
      "match_criteria": {"user_query": "running shoes"}
    }
  }
}'
```

`sku-1001` comes first and `sku-1003` is gone. Criteria can also be `fuzzy`, `prefix`, `suffix`,
`contains` or numeric (`lt`, `gte`, …) on any metadata you pass in `match_criteria`, such as a
country. The **Relevance** tab edits rules as sentences and tests a query against them. Rules can
pin and exclude; boosting, burying and redirects aren't there yet (`## Next` item 15 in
[../README.md](../README.md)).

## Learn what shoppers search for

Turn on search analytics for the index, and send the shopper's text in `X-Roll-Search-Query` with
each search so roll records what they typed:

```sh
es PUT /products/_settings '{"index.roll.analytics.enabled": true}'
```

The **Relevance** tab then shows the top queries, queries that found nothing and queries whose
results nobody clicks, each with buttons that open the synonym or query-rule editor prefilled.

## Record clicks

Clicks use Elasticsearch's behavioral analytics events. Create a collection once:

```sh
es PUT /_application/analytics/shop
```

Then, when a shopper clicks a result, your backend sends:

```sh
es POST /_application/analytics/shop/event/search_click '{
  "session": {"id": "sess-8f2c"},
  "user": {"id": "user-42"},
  "search": {"query": "running shoes"},
  "document": {"id": "sku-1001", "index": "products"}
}'
```

The click counts toward the query's click-through rate in the **Relevance** tab. Add
`search.results.items` and `page` when you have them and roll also records the click's position.
The key that sends events only needs the `post_behavioral_analytics_event` cluster privilege:

```sh
es POST /_security/api_key '{
  "name": "shop-click-events",
  "role_descriptors": {"events": {"cluster": ["post_behavioral_analytics_event"]}}
}'
```

Use its `encoded` value against the cluster's own host. On a shared address such as
`http://127.0.0.1:59200`, encode `<id>@shop:<api_key>` in base64 instead.

## Next steps

- Autocomplete from real searches: [site-search.md](site-search.md#suggest-queries-from-what-people-search).
- Semantic search for queries like "shoes for wet trails": [rag.md](rag.md), then mix it with
  this lexical query in an `rrf` retriever.
- Try a new mapping on a copy: `roll search clusters branch shop shop-test`, reindex there, and
  delete the branch when you're done.
