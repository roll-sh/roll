# Logs end to end: design

`## Next` item 18 in [README.md](README.md). Status: design, nothing built.

## Why

Logs are the biggest Elasticsearch workload, and the managed products sell the whole path:
- **Elastic Observability:** a managed OTLP endpoint, Discover with saved searches,
  visualizations, and alerting rules.
- **AWS:** OpenSearch Ingestion pipelines and the Alerting plugin's monitors.

roll serves the storage half well: data streams, lifecycles, ES|QL, Watcher, the built-in `logs`
template, and Elasticsearch's OpenTelemetry templates (`logs-*.otel-*`, `traces-otel`, the
`metrics-*.otel` family, `meta/otel_templates.json`). Today's guide (`guides/logs.md`) has the
customer run their own shipper.

roll apps' drain (#735) writes ECS documents into `logs-roll.apps-*` through `_bulk` with an
`append` key, so apps' logs already land.

Missing:
- an OTLP intake, so any OpenTelemetry SDK or Collector can point at a cluster;
- an alerting API OpenSearch customers know, and a UI for Watcher;
- alerts on a cluster's own health;
- saved searches and visualizations in Discover.

## 1. An OTLP intake on the cluster's endpoint

- **Routes.** On the data plane, beside Elasticsearch's API:
  - `POST /_otlp/v1/logs`, `/_otlp/v1/traces` and `/_otlp/v1/metrics`;
  - OTLP/HTTP with protobuf (`application/x-protobuf`) or JSON, gzip allowed.

  A Collector's `otlphttp` exporter points at `https://<cluster>.search.<domain>/_otlp` with an
  API key header. gRPC on its own port is phase 2.
- **Who may write.** A key with a new `otel` access level: `create_doc` and `auto_configure` on
  exactly `logs-*.otel-*`, `traces-*.otel-*` and `metrics-*.otel-*`, no cluster privileges.
  #677's `append` takes exact names only, so it can't cover OTel's dataset routing. Any other key
  with `create_doc` on the target works too. OTLP never creates indices outside those patterns.
- **The documents** are what Elastic's managed endpoint writes (OTel-native, not ECS), so Kibana's
  and roll's OTel views read them:
  - a log record becomes `@timestamp`, `observed_timestamp`, `severity_text`/`severity_number`,
    `body.text` (or `body.structured`), `attributes.*`, `resource.attributes.*`, `scope.*`,
    `trace_id`, `span_id`;
  - spans and metrics follow the same mapping;
  - the built-in OTel templates already map those fields.
- **Routing.** The target is `<type>-<dataset>.otel-<namespace>`. The dataset and namespace come
  from the `data_stream.dataset` / `data_stream.namespace` attributes, defaulting to `generic` and
  `default`. The names are validated: lowercase, no `-` in the dataset, length-capped.
- **Limits** ("charge what you build"):
  - the body is capped at the cluster's `max_body_bytes`, with gzip counted as decompressed;
  - protobuf decoding is charged to the request breaker as it allocates (`prost` into owned
    messages, at most a fixed multiple of the bytes, checked);
  - attributes are capped at 128 per record and their values at 32 KiB;
  - nesting is capped;
  - documents are written as one internal `_bulk` of `create` actions per request.
- **Answers.** Per the OTLP specification: `200` with `partial_success` naming rejected records.
  A `429` from the breaker or rate limit carries `Retry-After`, which Collectors back off on.
- **Metering.** Like any bulk write. Apps' drain could move to OTLP later. Its ECS stream keeps
  working either way.

## 2. Alerting OpenSearch customers know, and Watcher with a UI

- **Elasticsearch-flavor clusters** keep Watcher, which roll already runs on the owner node's
  scheduler. The dashboard gains a Watcher page: list, create from a template, run once, history.
- **OpenSearch-flavor clusters** get the Alerting plugin's routes mapped onto the same scheduler and
  execution as watches:
  - `_plugins/_alerting/monitors` with query, bucket and document monitors, triggers,
    acknowledgements and the `_execute` dry run;
  - `_plugins/_notifications/configs` with webhook, Slack and Chime channels.
- **Actions.** Both dialects' actions go out through roll's egress check: public addresses only,
  no redirects, and the deny lists from the egress fix in progress. Webhook secrets are sealed
  through the single `seal_given` path and never returned.
- **Chat ops.** A roll-native action, `roll_chatops`, posts to a Slack or Discord channel the owner
  already connected to roll code's chat ops, without copying the webhook URL into the cluster.

## 3. Alerts on a cluster's own health

- **The rules.** The control plane already keeps per-cluster request metrics (`GET
  …/metrics`: rates, p50/p95/p99, 5xx, 429s) and usage. Owners set rules there: `{metric, op,
  threshold, window}`, for example `p99 > 500 ms for 5 min` or `429s > 1%`. They live in roll's
  catalog, not in the cluster, so they work while the cluster sleeps and can't be edited through
  the cluster's API.
- **Delivery.** Rules deliver through roll's webhook delivery (signed, retried, with the same
  SSRF guard) and through chat ops.
- **The dashboard.** The Performance tab shows a rule's state next to its chart. Usage alerts
  ("this month's cost passes $X") reuse the same rules.

## 4. Discover: saved searches and visualizations

- **Saved searches.** A saved search stores a query (KQL or ES|QL), its time range, its columns
  and its index pattern, kept in a hidden index of the cluster (`.roll-saved-objects`). So it
  branches and snapshots with the cluster, and its query text, which is tenant data, never
  leaves it.
- **Visualizations.** Saved charts from ES|QL `STATS … BY` results: line, bar, metric and table.
  They're rendered by the dashboard's chart components and shared by URL within the project.
- **Imports.** Kibana's saved-objects import (`.ndjson`) is phase 3: index patterns and saved
  searches map directly, but Lens and TSVB visualizations don't, and the import lists what it
  dropped.

## Phases

1. **Phase 1:** the OTLP/HTTP intake (logs and traces), the `otel` access level, a logs guide section
   with an OpenTelemetry Collector config, and the Watcher page.
2. **Phase 2:**
   - health alerts on the control plane, delivered by webhook and chat ops;
   - saved searches;
   - OTLP metrics and gRPC;
   - the Alerting plugin's monitors on OpenSearch-flavor clusters.
3. **Phase 3:** visualizations, Kibana saved-object imports, and notification channels beyond
   webhook and chat.

## Tests

- **Documents:** search-es tests compare OTLP JSON and protobuf fixtures (from the
  OpenTelemetry proto repo's examples) with the documents Elastic's managed endpoint writes for
  them. Those are recorded once, and only shapes go in the repo.
- **Hostile bodies:** a gzip bomb, deep nesting, 10,000 attributes and a huge attribute value
  under a small breaker each get a 413 or 429, never growth.
- **Routes and rules:** search-control tests cover the intake's auth matrix and health rules
  firing and recovering. A collector config in the guide is run once against a local cluster in
  the guide's check.
