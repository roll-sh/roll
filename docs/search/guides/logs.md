# Log search

Application logs in roll search: a data stream that rolls over by itself, retention that deletes
old data, an ingest pipeline that parses raw lines, browsing in the dashboard, ES|QL queries and
snapshots. About fifteen minutes.

roll search stores logs in object storage and unloads a cluster nobody is querying, so a log
cluster that is written to all day but searched only during incidents pays for memory mostly
while it's written to and searched.

## What's missing

So you know before you start:

- **No OTLP intake.** roll search doesn't accept OpenTelemetry logs or traces directly. Send logs
  with `_bulk` (below), or with a shipper that speaks the Elasticsearch API (Filebeat, Fluent Bit,
  Vector, Logstash, the OpenTelemetry Collector's Elasticsearch exporter).
- **No alerting UI.** Watcher's API is served but has no screen, OpenSearch's Alerting plugin isn't
  served, and Discover-style browsing has no saved searches or visualizations.

Both are `## Next` item 18 in [../README.md](../README.md).

## Before you start

You need a cluster, its URL and a key, as in [the first query](README.md#the-first-query-in-a-minute).
Snapshot policies need an admin key, so use one for setup; your shippers can use `append` keys.

```sh
roll search clusters create logs
roll search keys create logs setup --access admin --expires-in 1d
export ELASTICSEARCH_URL=…        # printed by the commands above
export ELASTICSEARCH_API_KEY=…
```

The requests use the `es` helper from [the guides' README](README.md#a-shorter-way-to-send-requests).

## Parse lines with an ingest pipeline

Your app writes lines like `2026-11-21T10:04:12Z GET /checkout 200 31.5`. This pipeline splits
them into fields with `grok` and sets `@timestamp` from the line with `date`:

```sh
es PUT /_ingest/pipeline/app-logs '{
  "description": "Parse access lines",
  "processors": [
    {"grok": {
      "field": "message",
      "patterns": ["%{TIMESTAMP_ISO8601:ts} %{WORD:http.method} %{URIPATHPARAM:url.path} %{NUMBER:http.status:int} %{NUMBER:duration_ms:float}"]
    }},
    {"date": {"field": "ts", "formats": ["ISO8601"], "target_field": "@timestamp"}},
    {"remove": {"field": "ts"}}
  ],
  "on_failure": [
    {"set": {"field": "error.message", "value": "{{ _ingest.on_failure_message }}"}}
  ]
}'
```

Try it on a line before using it:

```sh
es POST /_ingest/pipeline/app-logs/_simulate '{
  "docs": [{"_source": {"message": "2026-11-21T10:04:12Z GET /checkout 200 31.5"}}]
}'
```

A line that doesn't match keeps its `message` and gets `error.message`, instead of being dropped.
The dashboard's **Pipelines** tab lists pipelines, edits them and runs the same simulation.

## Keep 30 days with a lifecycle policy

```sh
es PUT /_ilm/policy/logs-30d '{
  "policy": {
    "phases": {
      "hot": {"actions": {"rollover": {"max_age": "1d", "max_primary_shard_size": "50gb"}}},
      "delete": {"min_age": "30d", "actions": {"delete": {}}}
    }
  }
}'
```

Each day (or at 50 GB) the data stream starts a new backing index; a backing index is deleted 30
days after it rolled over. A sleeping cluster is woken when a lifecycle step is due, then sleeps
again.

If you only need retention, the data stream lifecycle is simpler: put
`"lifecycle": {"data_retention": "30d"}` in the template below instead of the ILM setting.

On an OpenSearch-flavor cluster use Index State Management instead:

```sh
es PUT /_plugins/_ism/policies/logs-30d '{
  "policy": {
    "description": "Roll over daily, delete after 30 days",
    "default_state": "hot",
    "states": [
      {"name": "hot", "actions": [{"rollover": {"min_index_age": "1d"}}],
       "transitions": [{"state_name": "delete", "conditions": {"min_index_age": "30d"}}]},
      {"name": "delete", "actions": [{"delete": {}}], "transitions": []}
    ],
    "ism_template": [{"index_patterns": ["logs-myapp-*"], "priority": 100}]
  }
}'
```

## Create a data stream template

A data stream is an append-only name (`logs-myapp-default`) backed by indices that roll over. An
index template says which names become data streams and how their indices look:

```sh
es PUT /_index_template/logs-myapp '{
  "index_patterns": ["logs-myapp-*"],
  "data_stream": {},
  "priority": 500,
  "template": {
    "settings": {
      "index.lifecycle.name": "logs-30d",
      "index.default_pipeline": "app-logs"
    },
    "mappings": {
      "properties": {
        "@timestamp":  {"type": "date"},
        "message":     {"type": "text"},
        "http.method": {"type": "keyword"},
        "url.path":    {"type": "keyword"},
        "http.status": {"type": "short"},
        "duration_ms": {"type": "float"}
      }
    }
  }
}'
```

`priority: 500` puts it above the built-in `logs-*-*` template. `index.default_pipeline` runs
`app-logs` on every document, so shippers don't have to name it. The **Indices** tab lists index
templates and data streams.

## Send logs with _bulk

Data streams only take new documents, so each action line is `create`. Save as `batch.ndjson`:

```json
{"create": {}}
{"message": "2026-11-21T10:04:12Z GET /checkout 200 31.5"}
{"create": {}}
{"message": "2026-11-21T10:04:13Z POST /api/cart 500 812.0"}
{"create": {}}
{"message": "2026-11-21T10:04:15Z GET /health 200 0.4"}
```

```sh
es POST /logs-myapp-default/_bulk @batch.ndjson
```

The first write creates the data stream. Check the answer's `errors` flag; a shipper retries
items that failed with 429. For high volume, batch a few thousand lines or a few MB per call. A
cluster accepts bodies up to `max_body_mb` (100 MB by default; `--max-body-mb` on
`roll search clusters update`).

## Browse logs in the dashboard

Open the cluster's **Indices** tab and pick `logs-myapp-default` under data streams. A time-based
index gets a time range (presets or your own), a histogram of documents over time like Kibana's
Discover, and the documents newest first, with a query string box (`http.status:>=500`) and a
column picker. Any view opens in the **Console** as a request.

## Query with ES|QL

ES|QL is a piped query language for exploring and aggregating. Errors in the last hour, by path:

```sh
es POST /_query '{
  "query": "FROM logs-myapp-default | WHERE @timestamp > NOW() - 1 hour AND http.status >= 500 | STATS errors = COUNT(*) BY url.path | SORT errors DESC | LIMIT 10"
}'
```

Slowest paths by 95th percentile:

```sh
es POST /_query '{
  "query": "FROM logs-myapp-default | STATS p95 = PERCENTILE(duration_ms, 95), hits = COUNT(*) BY url.path | SORT p95 DESC | LIMIT 10"
}'
```

The answer is `columns` and `values`. Add `?format=txt` for a table you can read in a terminal.
The **Console** tab runs ES|QL too. Plain `_search` with aggregations (`date_histogram`, `terms`,
`percentiles`) works as on Elasticsearch, if your tools already speak it.

## Take snapshots

Every Elasticsearch-flavor cluster has a snapshot repository, `found-snapshots`, kept in the
cluster's own storage. Take a snapshot every night and keep 30 days of them:

```sh
es PUT /_slm/policy/nightly '{
  "schedule": "0 30 1 * * ?",
  "name": "<nightly-{now/d}>",
  "repository": "found-snapshots",
  "config": {"indices": ["logs-*"], "include_global_state": false},
  "retention": {"expire_after": "30d", "min_count": 5, "max_count": 50}
}'

es POST /_slm/policy/nightly/_execute      # take one now
es GET /_snapshot/found-snapshots/_all
```

To restore a data stream from a snapshot, delete it (or restore it under a new name with
`rename_pattern` and `rename_replacement`) and call
`POST /_snapshot/found-snapshots/<snapshot>/_restore` with `{"indices": "logs-myapp-default"}`.
The **Snapshots** tab lists snapshots and policies and restores from them. You can also register
your own S3, GCS or Azure repository with `PUT /_snapshot/<name>`.

For "what did the logs look like an hour ago" you often don't need a snapshot: branch the cluster
as it was then (within the history window, a day by default) and query the branch:

```sh
roll search clusters branch logs logs-before-incident --at 1h
```

## Next steps

- Watch volume and latency: the **Performance** tab shows requests, errors, 429s, p50/p95/p99
  latency and the slow logs; `roll search clusters usage logs` shows the cost.
- Keep shippers' keys narrow: an `append` key per shipper, which adds to its data stream and can't
  read it or write anywhere else
  (`roll search keys create logs fluent-bit --access append --indices logs-myapp-default`), revoked
  on its own.
