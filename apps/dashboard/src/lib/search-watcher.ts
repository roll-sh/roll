// The Watcher page's logic (Elasticsearch's alerting, served by roll on the owner node's
// scheduler): watches as `_watcher/_query/watches` lists them, starter watches built from a
// template, and a watch's runs from `.watcher-history-*`.

export interface WatchStatus {
  state?: { active?: boolean; timestamp?: string };
  last_checked?: string;
  last_met_condition?: string;
  execution_state?: string;
  actions?: Record<string, { ack?: { state?: string }; last_execution?: { successful?: boolean; reason?: string } }>;
}

export interface WatchRow {
  _id: string;
  watch: { trigger?: { schedule?: Record<string, unknown> }; input?: Record<string, unknown>; condition?: Record<string, unknown>; actions?: Record<string, unknown>; metadata?: Record<string, unknown> };
  status: WatchStatus;
}

export interface WatchRun {
  id: string;
  triggered: string | null;
  state: string;
  met: boolean | null;
  /** The first action's outcome or the run's own failure. */
  note: string | null;
}

/** A schedule in words: `every 5m`, `cron 0 0 * * *`, `daily at 09:00`. */
export function scheduleText(schedule: Record<string, unknown> | undefined): string {
  if (!schedule) return "—";
  const [kind, v] = Object.entries(schedule)[0] ?? [];
  if (!kind) return "—";
  if (kind === "interval") return `every ${String(v)}`;
  if (kind === "cron") return `cron ${Array.isArray(v) ? v.join(" | ") : String(v)}`;
  if (kind === "daily" || kind === "hourly" || kind === "weekly" || kind === "monthly" || kind === "yearly") return kind;
  return kind;
}

/** Where a watch stands, in a word: `paused`, `failing`, `firing`, `ok` or `not run yet`. */
export function watchState(s: WatchStatus): "paused" | "failing" | "firing" | "ok" | "not run yet" {
  if (s.state?.active === false) return "paused";
  if (Object.values(s.actions ?? {}).some((a) => a.last_execution?.successful === false)) return "failing";
  if (!s.last_checked) return "not run yet";
  if (s.last_met_condition && s.last_met_condition === s.last_checked) return "firing";
  return "ok";
}

export type TemplateKind = "errors" | "silence";

export interface TemplateInput {
  kind: TemplateKind;
  /** Index, alias, data stream or pattern the watch counts in. */
  index: string;
  /** `errors`: fire above this many matching documents in the window. */
  threshold: number;
  /** How often it runs and how far back it looks, as Elasticsearch's time units (`5m`). */
  interval: string;
  /** `errors`: the KQL-free query string matched (`log.level:error`). */
  query: string;
  /** Post here when it fires; empty: write a log line only. */
  webhook: string;
}

export const TEMPLATES: { kind: TemplateKind; label: string; hint: string }[] = [
  { kind: "errors", label: "Too many matching documents", hint: "Counts documents matching a query in the last interval and fires above a threshold." },
  { kind: "silence", label: "No new documents", hint: "Fires when nothing arrived in the last interval: a shipper or app stopped sending." },
];

/** A watch body from a starter template, in Watcher's own API shape. */
export function watchFromTemplate(t: TemplateInput): Record<string, unknown> {
  const range = { range: { "@timestamp": { gte: `now-${t.interval}` } } };
  const must = t.kind === "errors" && t.query.trim() ? [range, { query_string: { query: t.query.trim() } }] : [range];
  const compare =
    t.kind === "errors" ? { "ctx.payload.hits.total": { gt: Math.max(0, Math.floor(t.threshold)) } } : { "ctx.payload.hits.total": { eq: 0 } };
  const text =
    t.kind === "errors"
      ? `{{ctx.payload.hits.total}} documents in ${t.index} matched in the last ${t.interval}`
      : `no new documents in ${t.index} in the last ${t.interval}`;
  const actions: Record<string, unknown> = { log: { logging: { text: `Watch {{ctx.watch_id}}: ${text}` } } };
  if (t.webhook.trim()) {
    actions.notify = {
      webhook: { method: "post", url: t.webhook.trim(), body: `{"watch":"{{ctx.watch_id}}","text":"${text.replace(/"/g, '\\"')}"}`, headers: { "Content-Type": "application/json" } },
    };
  }
  return {
    trigger: { schedule: { interval: t.interval } },
    input: { search: { request: { indices: [t.index], rest_total_hits_as_int: true, body: { size: 0, query: { bool: { filter: must } } } } } },
    condition: { compare },
    actions,
    metadata: { roll_template: t.kind },
  };
}

/** Elasticsearch's time units the templates take: `30s`, `5m`, `1h`, `1d`. */
export const INTERVAL_RE = /^[1-9]\d{0,4}[smhd]$/;

/** A watch id Watcher takes and a person can read. */
export const WATCH_ID_RE = /^[a-z0-9][a-z0-9_-]{0,63}$/;

/** The search of a watch's last runs (newest first) in its history. */
export function historyQuery(watchId: string, size = 20): Record<string, unknown> {
  return { size, sort: [{ "trigger_event.triggered_time": "desc" }], query: { term: { watch_id: watchId } } };
}

/** History hits as runs. */
export function runsOf(hits: { _id: string; _source?: Record<string, unknown> }[]): WatchRun[] {
  return hits.map((h) => {
    const s = (h._source ?? {}) as Record<string, any>;
    const action = Array.isArray(s.result?.actions) ? s.result.actions[0] : undefined;
    const note = s.messages?.[0] ?? action?.reason ?? (action ? `${action.id}: ${action.status}` : null);
    return {
      id: h._id,
      triggered: s.trigger_event?.triggered_time ?? s.result?.execution_time ?? null,
      state: String(s.state ?? "unknown"),
      met: typeof s.result?.condition?.met === "boolean" ? s.result.condition.met : null,
      note: note == null ? null : String(note),
    };
  });
}
