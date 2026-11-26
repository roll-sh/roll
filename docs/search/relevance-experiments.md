# Relevance evaluation, A/B tests and merchandising: design

`## Next` item 15 in [README.md](README.md). Status: two slices of section 3 (live A/B tests, with
results and promotion) and of section 2 (conversion events, browser keys' events, conversions in
the indices' analytics) are built: [First slice as built](#first-slice-as-built).
The rest is design.

## First slice as built

Code: `crates/search-es/src/xpack/entsearch/experiments.rs` (configuration, assignment, events),
`crates/search-es/src/analytics.rs` (the counts), `crates/search-control/src/analytics.rs` (the
results and the z-test), the Relevance tab's Experiments (`components/search/analytics.tsx`).
Where this design or the request left a choice open, the stricter one:

- **Where an experiment lives.** On the search application itself, as roll's extension of its
  `PUT`: `experiments: [{id, variants: [{name, weight, params? | template?}], ends_at?, allow_user_header?}]`
  (in place of section 3's control route, so Elasticsearch's own API carries it and branches,
  snapshots and restores keep it). At most 10 are kept, ended ones included, and at most one runs
  (no `ends_at`, or one ahead, in unix milliseconds). A running experiment's variants, weights and
  `allow_user_header` can't change (400): end it and start another, or its counts would mix two
  tests. 2 to 8 variants, weights 1 to 10,000; a variant's `params` are checked by the
  application's `dictionary` when it's put and win over the request's; a `template` replaces the
  application's (its `dictionary` still checks requests).
- **Who gets which.** A stable hash (SHA-256) of the experiment and the visitor's identity, split
  by weight. The identity is a derived key's `user` (signed, so nothing a page sends overrides it),
  else the `X-Roll-User` header only when the experiment sets `allow_user_header` (off by
  default), else the client's address. Without any of them the application's own template runs and
  nothing is counted. The identity is hashed and never stored. With `X-Roll-User` allowed, a
  visitor can rotate identities to add trials to whichever arm their new names land in; each
  needs a search of its own first, so the key's rate limits bound how many.
- **The answer says which** in headers, `X-Roll-Experiment` and `X-Roll-Variant`, exposed to pages
  by CORS: the body stays Elasticsearch's, so no client needs to ask (section 3's `roll.variant`).
- **What's counted.** A *trial* is one visitor's query in an hour (searching it again or paging
  counts once). A `search_click` or `roll_conversion` naming the application in
  `search.search_application` counts at most once per trial, and only for a trial that same
  identity made in this hour or the last: a visitor can add a click only to their own search, so
  one can't buy a variant's numbers (in place of section 2's per-address cap). Trials are 64-bit
  keys held in memory for two hours (200,000 an hour, then new ones aren't counted), never handed
  on. The counts are per variant, since the experiment started, whatever the analytics range, and
  counted whether or not an index has analytics on: they hold no query text.
- **`roll_conversion`** takes `{session, user, search, document?, value?, currency?}`, `value`
  0 to 1e12 and `currency` 1 to 8 characters. It isn't added to the indices' click analytics yet.
- **Browser keys' `limits.events`** (off by default): `search_click` and `roll_conversion` only,
  at most 4 KiB, about a document of an index the key reads or that its search application
  searches, or the application itself; counted for the key's own visitor and never stored in the
  collection's data stream. A key without it is refused (400). It needs no cluster privilege: the
  event API joins the `search_application_query` workflow for the purpose.
- **Results** (`GET …/analytics`'s `experiments`, the Relevance tab): per variant its trials,
  clicks, conversions, their value, click-through and conversion rate, and against the baseline
  (the variant named `control`, else the first by name) a two-sided two-proportion z-test with
  the pooled standard error: `significant` below p 0.05, `not_yet_significant` above it, and
  `not_enough_data` until both arms have 100 trials with five successes and five failures.
- **Second slice** (also built):
  - **Conversions in the indices' analytics.** A `roll_conversion` about a document of an index
    with analytics on counts against the event's query in that index's hour, like a click, with
    its value (in thousandths, summed as given): `conversions`, `conversion_rate` and `value` in
    `GET …/analytics`'s totals and query rows. Like clicks there, every event counts; the
    per-variant counts keep their once-per-trial rule.
  - **Zero results and latency per variant.** A trial whose first search found nothing counts in
    `no_hits` (`no_hit_rate`); every search of the variant is timed into a histogram of 16
    buckets (under 1, 2, 4 … ms), so nodes and passes add up exactly, and `p95_latency_ms` is the
    bound of the bucket holding the 95th percentile.
  - **Promoting a winner.** `POST _application/search_application/{name}/_roll/experiments/{id}/_promote`
    with `{"variant"}`, authorized as the application's `PUT`: the variant's params go over the
    template's own params, or its template takes the application's place (the `dictionary`
    stays); the experiment ends now if it still runs and records `promoted: {variant, at}`. Once
    per experiment (409 again). An ended experiment can't be started again by a `PUT` (400): its
    counts would mix two tests.
- **Intervals and promoting from the tab** (also built): each rate carries its 95% Wilson
  interval, and each comparison the difference from the baseline with its 95% interval
  (Newcombe's hybrid score interval, from the two Wilson intervals, so it agrees with them at
  small counts and rates near 0 where the plain normal interval doesn't). The Relevance tab shows
  them under each rate, reads each experiment's state from its application (running, ended,
  promoted) and promotes a variant through the `_promote` route after saying what it does.
- **Left:** judgment lists and evaluation (section 1), the `roll_` rule types (section 4) and
  dynamic re-ranking (section 5).

## Why

People who run a search box measure it and change it in a loop. Algolia sells that loop:
- A/B tests split live traffic between index configurations and report click-through and
  conversion with a confidence figure;
- rules boost, bury, filter, redirect and return banners;
- conversion events feed analytics and dynamic re-ranking.

OpenSearch 3.x ships its Search Relevance Workbench, with query sets, judgments (implicit from
clicks, or from an LLM) and experiments.

roll has the pieces but not the loop:
- **Evaluation:** `_rank_eval` with no judgment lists or UI.
- **Analytics:** per-index search analytics (searches, zero results, `search_click` events,
  click-through) and suggestions ranked by clicks.
- **Rules:** query rules that stop at Elasticsearch's `pinned` and `exclude`.
- **Applications:** search applications, a stored template behind one name, which #358 made the
  safe surface for browser keys.
- **Playground:** compares two queries offline.

This design joins them, with no break in Elasticsearch compatibility: every action Elasticsearch
lacks is a roll extension under a `roll_` name, and Elasticsearch's own APIs keep answering as
9.5.5 does.

## 1. Judgments and offline evaluation

**First slice as built** (`feature/search-evaluate`): `POST /v1/search/clusters/{id}/indices/{index}/evaluate`
(`crates/search-control/src/evaluate.rs`) takes the judgments in the request, `[{query, ratings:
[{_id, rating, _index?}]}]` (up to 500 queries of 200 ratings), and one or two configurations:
a search body with `{{query}}` where each query goes, or a search template (a stored one by `id`,
or an inline `source`, with `params`). Each configuration is one `_rank_eval` on the index with
the judged queries as its templated requests, run as the index's routes run (a read), so the
cluster checks and meters them as any search; the metric is `_rank_eval`'s, nDCG@10 by default.
With two, the second is compared against the first: the difference in score, how many queries
got better, worse or stayed, and the 50 that moved most each way. `roll search index evaluate`
reads judgments as JSON or CSV (`query,_id,rating`, Quepid's export) and fails a CI run with
`--fail-below` or `--fail-on-worse`; MCP's `search_evaluate` maps the route; the Relevance tab's
Evaluate panel takes a paste or a file and prints the CI line. Next: judgment lists kept as
indices and built from clicks (below), and evaluating a search application's own template and
its experiment's variants.

- **Judgment lists.** A judgment list is an ordinary index the owner names
  (`roll-judgments-<name>`): documents `{query, index, doc_id, rating, source}`, with `source`
  being `manual`, `clicks` or `import`. It's a plain index, so it branches, snapshots and exports
  like any other, and nothing new stores tenant data.
- **Clicks to judgments.** `POST /v1/search/clusters/{id}/judgments/{name}/_from_clicks` builds
  ratings from the index's click analytics. It uses clicks over expected clicks at each position
  (COEC, as OpenSearch's implicit judgments do), only for queries with at least N searches, and
  stores the counts it used. Analytics keeps only counts, so no user identifier enters a judgment.
- **Evaluating.** The Relevance tab's "Evaluate" takes a judgment list and one or two search
  configurations: a search application's template or a request body. It runs Elasticsearch's
  `_rank_eval` (nDCG@k, MRR, precision@k, recall@k) for each, and shows the metric per query with
  the queries that got better or worse, linked into the Playground.
  `POST …/evaluations` runs it from the API and the CLI (`roll search evaluate`), for CI: a
  pipeline can fail a relevance change that lowers nDCG.
- **OpenSearch-flavor clusters** get the Search Relevance Workbench's own routes
  (`_plugins/_search_relevance/query_sets`, `judgments`, `search_configurations`, `experiments`)
  over the same machinery. A design pass against `opensearchproject/opensearch:3.9.0` comes before
  they're built.

## 2. Conversion events

Conversions are what A/B tests and dynamic re-ranking need, and roll doesn't record them today.

- **The event.** Elasticsearch's behavioral analytics events stay as they are (`search`,
  `search_click`, `page_view`). roll adds the event type `roll_conversion` to the same collection
  route: `{search.query, document, value?, currency?}`. It's counted like a click, against the
  query that led to it. Elasticsearch answers an unknown event type with a 400, and so does roll
  for any other name.
- **From the page.** Today a page has to post events through its backend: a `data` key can't post
  them, as the guides found. Browser keys gain `limits.events: true`. The key may then post
  `search_click` and `roll_conversion` events to the collections of the indices it reaches, within
  its own rate limits and per-address limits, with bodies capped at 4 KiB.
- **Hostile events.** Events from a page are untrusted. They're counted, never stored raw. A
  document id has to belong to an index the key reaches. Counts are capped per address per query
  per day, so one visitor can't buy a ranking.

## 3. Live A/B tests on search applications

- **An experiment.** An experiment belongs to a search application:
  `PUT /v1/search/clusters/{id}/applications/{app}/experiment` with
  `{variants: [{name, template}, {name, template}], split: [50, 50], ends_at?}`.
  - While it runs, `_application/search_application/{app}/_search` picks a variant per user: a
    stable hash of the `X-Roll-User` header (a token the page sets) or, without one, of the
    client's address.
  - It renders that variant's template and records the variant on the search, click and
    conversion counts.
  - Results carry `roll.variant` only when the request asks for it, so ES clients see Elasticsearch's
    answer.
- **Results.** The Relevance tab shows per variant: searches, click-through, conversion rate,
  zero-result rate and p95 latency. Each has a two-proportion confidence interval, and a call
  ("B is better with 95% confidence") once both arms pass a minimum sample. Ending an experiment
  can promote the winner to the application's template, written like any template change.
- **Limits.** One experiment per application. Only the owner (roll-access `write` on the cluster)
  starts or ends one. Variants are templates the owner wrote, so a browser key's policy still
  applies to whatever a variant renders.

## 4. Merchandising rules

Elasticsearch's query rules (`_query_rules`, the `rule` retriever) have two rule types, `pinned`
and `exclude`, and roll keeps both exactly. roll adds rule types under its own names. Elasticsearch
refuses unknown types, and roll accepts these only as named here:

| type | actions | Algolia |
|---|---|---|
| `roll_boost` | `{filter, weight}`: matching documents' scores multiplied (a `function_score` wrapped around the organic query) | promote by filter, "boost" |
| `roll_bury` | `{filter, weight < 1}` | "bury" |
| `roll_filter` | `{filter}` added as a filter clause | consequence filters |
| `roll_redirect` | `{url}` returned in the response's `ext.roll.redirect` | redirects |
| `roll_data` | `{data}` returned in `ext.roll.user_data` | `userData` banners |

- **Criteria.** Rules use Elasticsearch's criteria (`exact`, `fuzzy`, `prefix`, `contains`,
  `always`, and so on). They apply only through the `rule` retriever or a search application
  that uses it, so a plain `_search` is never changed by surprise.
- **Editing.** The Relevance tab edits these rules next to synonyms. MCP tools follow the routes,
  as for the plain rulesets #677 added.
- **Exports.** A ruleset holding roll types is an Elasticsearch ruleset with extensions. Exporting
  to Elasticsearch drops the `roll_` rules and lists them.

## 5. Dynamic re-ranking

This is Algolia's dynamic re-ranking: reorder the top results for a frequent query by what people
click and buy.
- **Switching it on.** It's an index setting, `index.roll.dynamic_reranking: {enabled, strength,
  min_searches}`, and needs analytics on.
- **What it does.** For a query with enough searches, the top `window` hits (100 by default) are
  re-scored by `score × (1 + strength × ctr_or_cr(query, doc))` from the counts analytics already
  keeps.
- **Where it applies.** Only through search applications and the `rule` retriever, as section 4
  does. The response marks re-ranked hits when asked. Turning it off is immediate.

## Security and limits, all together

- **Writes need write.** Experiments, rules, judgment lists and settings need roll-access `write`
  on the cluster. `_from_clicks` needs read on analytics.
- **Browser keys** can post events (with `limits.events`), never start experiments or write rules.
- **No identifiers kept.** A user's token is hashed for assignment and never stored.
- **Bounded memory.** Evaluations run as tasks under the node's time cap (#530) and memory
  account. A `_rank_eval` over a large judgment list pages through it.

## Phases

1. **Phase 1:**
   - judgment lists (manual, from clicks, imported);
   - offline evaluation in the Relevance tab, the API and the CLI;
   - `roll_conversion` events and browser keys' `limits.events`.
2. **Phase 2:** live A/B tests on search applications with results and promotion.
3. **Phase 3:**
   - the `roll_` rule types;
   - dynamic re-ranking;
   - the OpenSearch Search Relevance Workbench routes.

## Tests

- **Routes:** search-control `control.rs` tests for judgments from clicks, an experiment's
  assignment staying stable per token, its counts per variant, and promotion.
- **Engine:** search-es tests for each `roll_` rule type through the `rule` retriever, an unknown
  type's 400 matching Elasticsearch, `_rank_eval` across both configurations, and events refused
  for documents outside the key.
- **Dashboard:** vitest for the confidence interval and the significance call.
